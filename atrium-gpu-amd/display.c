/*
 * display.c — the D-display-1 scanout path (one connector / one CRTC).
 *
 * The display block is architecturally independent of the GFX/compute engine:
 * its own register aperture (APER_DISP). These handlers drive the discovery +
 * modeset + flip protocol the model (engine/src/display.rs) refs with a strict
 * referee — read the EDID byte-by-byte over DDC, program a VRAM BO as the
 * scanout framebuffer, flip (vsync on = latch at vblank, off = tear), and read
 * back vblank / tear / fault status. Scanout FBs are VRAM-resident BOs read by
 * the display at their VRAM offset (bo->pages[0]); this driver never walks
 * GPUVM for scanout.
 */
#include "atrium_gpu_amd.h"
#include "atrium_gpu_amd_abi.h"

/*
 * QUERY_CONNECTORS: report HPD state and read the full EDID base block over the
 * modeled DDC (I2C) register handshake — write each byte offset, read its byte.
 * A disconnected connector floats the DDC high (0xff), so the EDID the caller
 * parses then fails its header/checksum (the realistic failure mode).
 */
int
amd_display_query(struct atrium_amd_softc *sc, struct atrium_gpu_display_query *q)
{
	uint32_t i;

	q->connected = amd_mmio_read32(sc, regDISP_CONNECTOR_STATUS) & 1u;
	q->connector_type = amd_mmio_read32(sc, regDISP_CONNECTOR_TYPE);
	q->usbc_lanes = amd_mmio_read32(sc, regDISP_USBC_LANES);
	for (i = 0; i < ATRIUM_AMD_EDID_LEN; i++) {
		amd_mmio_write32(sc, regDISP_DDC_OFFSET, i);
		q->edid[i] = (uint8_t)amd_mmio_read32(sc, regDISP_DDC_DATA);
	}
	q->edid_len = ATRIUM_AMD_EDID_LEN;
	return (0);
}

/*
 * Program FB_BASE/FB_SIZE from a VRAM BO fd. The display reads VRAM by offset,
 * so the FB base is the BO's first VRAM page (bo->pages[0]). A System/GTT BO is
 * rejected here (the referee would also fault it as non-resident).
 */
static int
amd_display_program_fb(struct atrium_amd_softc *sc, struct thread *td, int fb_fd)
{
	struct atrium_amd_bo *bo;
	struct file *fp;
	uint64_t base;
	int err;

	err = amd_bo_fget(td, fb_fd, &fp, &bo);
	if (err != 0)
		return (err);
	if (!bo->vram) {
		fdrop(fp, td);
		return (EINVAL); /* scanout FBs must be VRAM-resident */
	}
	base = bo->pages[0];
	amd_mmio_write32(sc, regDISP_FB_BASE_LO, (uint32_t)base);
	amd_mmio_write32(sc, regDISP_FB_BASE_HI, (uint32_t)(base >> 32));
	amd_mmio_write32(sc, regDISP_FB_SIZE, (uint32_t)bo->size);
	fdrop(fp, td);
	return (0);
}

/* SET_MODE: program the connector's EDID mode with `fb_fd` as initial scanout. */
int
amd_display_set_mode(struct atrium_amd_softc *sc, struct thread *td, int fb_fd,
    uint32_t *fault)
{
	int err;

	err = amd_display_program_fb(sc, td, fb_fd);
	if (err != 0)
		return (err);
	amd_mmio_write32(sc, regDISP_SET_MODE, 1);
	*fault = amd_mmio_read32(sc, regDISP_FAULT);
	return (0);
}

/* FLIP: scan out `fb_fd` (vsync on = latch at vblank, off = take effect now). */
int
amd_display_flip(struct atrium_amd_softc *sc, struct thread *td, int fb_fd,
    uint32_t vsync, uint32_t *fault)
{
	int err;

	err = amd_display_program_fb(sc, td, fb_fd);
	if (err != 0)
		return (err);
	amd_mmio_write32(sc, regDISP_FLIP, vsync & 1u);
	*fault = amd_mmio_read32(sc, regDISP_FAULT);
	return (0);
}

/* STATUS: vblank count, dropped-flip count, current tear scanline (debug). */
void
amd_display_status(struct atrium_amd_softc *sc,
    struct atrium_gpu_display_status *st)
{
	st->vblank_count = amd_mmio_read32(sc, regDISP_VBLANK_COUNT);
	st->dropped_flips = amd_mmio_read32(sc, regDISP_DROPPED_FLIPS);
	st->tear_line = amd_mmio_read32(sc, regDISP_TEAR_LINE);
}

/* CONFIG: re-cable / re-plug the simulated monitor (bring-up / test). */
void
amd_display_config(struct atrium_amd_softc *sc, uint32_t ctype, uint32_t plug_mode)
{
	amd_mmio_write32(sc, regDISP_CFG_CONNECTOR_TYPE, ctype);
	amd_mmio_write32(sc, regDISP_CFG_PLUG_MODE, plug_mode);
}

/* USB-C: enter/exit DP Alt Mode (lanes 0 = USB/virtual, 2|4 = alt-mode). */
void
amd_display_usbc(struct atrium_amd_softc *sc, uint32_t lanes)
{
	amd_mmio_write32(sc, regDISP_CFG_USBC, lanes);
}
