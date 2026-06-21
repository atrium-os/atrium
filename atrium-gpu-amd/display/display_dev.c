/*
 * display_dev.c — atrium_gpu_amd_display.ko, the §4.1 display module.
 *
 * The display engine (DCN) is a distinct IP block from the GFX/compute engine:
 * its own register aperture (APER_DISP), its own driver, its own cdev
 * (/dev/atrium-display0). It attaches to the "atrium_gpu_amd_display" child the
 * base (pci) module creates, sharing that module's softc (BAR map + mmio
 * accessors + regDISP_* defs) but NOTHING of the GPU module — it loads, attaches
 * and serves independently of whether the GPU came up.
 *
 * It drives the discovery + modeset + flip protocol the model
 * (engine/src/display.rs) refs with a strict referee: read the EDID byte-by-byte
 * over DDC, decode its detailed timing, program a VRAM range as the scanout
 * framebuffer, flip (vsync on = latch at vblank, off = tear), and read back
 * vblank / tear / fault status. The scanout buffer arrives dma-buf-style as a
 * plain {vram_offset, size} (the compositor exported it from a GPU VRAM BO via
 * ATRIUM_GPU_IOC_BO_EXPORT_SCANOUT) — this module never touches the GPU's BO
 * table, it just programs FB_BASE/FB_SIZE and lets the referee enforce
 * residency.
 */
#include "atrium_gpu_amd.h"		/* shared softc + mmio accessors + regDISP_* */
#include "atrium_display_abi.h"

#include <sys/module.h>
#include <sys/kernel.h>

/*
 * The display block hangs off the shared softc (the base module's). Reach it the
 * same way the GPU child does: device_get_softc(parent). sc->dev is the PCI
 * device, so the mmio accessors (which use sc->regs) work unchanged.
 */
static struct atrium_amd_softc *
display_softc(struct cdev *cdev)
{
	return (cdev->si_drv1);
}

/* --- register helpers (formerly display.c, now (base,size)-based) --------- */

/*
 * ENUM: report HPD + §8 type and read the full EDID base block over the modeled
 * DDC (I2C) — write each byte offset, read its byte. A disconnected connector
 * floats the DDC high (0xff), so the EDID the caller parses then fails its
 * header/checksum (the realistic "unplugged" failure mode).
 */
static void
display_enum(struct atrium_amd_softc *sc, struct atrium_display_connector *c)
{
	uint32_t i;

	c->connected = amd_mmio_read32(sc, regDISP_CONNECTOR_STATUS) & 1u;
	c->connector_type = amd_mmio_read32(sc, regDISP_CONNECTOR_TYPE);
	c->usbc_lanes = amd_mmio_read32(sc, regDISP_USBC_LANES);
	for (i = 0; i < ATRIUM_DISPLAY_EDID_LEN; i++) {
		amd_mmio_write32(sc, regDISP_DDC_OFFSET, i);
		c->edid[i] = (uint8_t)amd_mmio_read32(sc, regDISP_DDC_DATA);
	}
	c->edid_len = ATRIUM_DISPLAY_EDID_LEN;
}

/*
 * Decode one 18-byte EDID Detailed Timing Descriptor into a mode. Mirrors the
 * model's edid.rs decode_dtd: pixel clock at 10 kHz granularity (bytes 0-1),
 * active/blanking split high nibbles across bytes 4 and 7. A zero pixel clock is
 * a monitor descriptor (name/range), not a timing — returns 0. Refresh is
 * derived = pixclk / (htotal * vtotal), reported in milli-Hz.
 */
static int
display_decode_dtd(const uint8_t *d, struct atrium_display_mode *m)
{
	uint32_t pc10k, pixclk, hactive, hblank, vactive, vblank;
	uint32_t htotal, vtotal;

	pc10k = (uint32_t)d[0] | ((uint32_t)d[1] << 8);
	if (pc10k == 0)
		return (0);		/* not a timing descriptor */
	pixclk = pc10k * 10000u;	/* Hz */

	hactive = (uint32_t)d[2] | (((uint32_t)d[4] >> 4) << 8);
	hblank = (uint32_t)d[3] | (((uint32_t)(d[4] & 0x0f)) << 8);
	vactive = (uint32_t)d[5] | (((uint32_t)d[7] >> 4) << 8);
	vblank = (uint32_t)d[6] | (((uint32_t)(d[7] & 0x0f)) << 8);
	if (hactive == 0 || vactive == 0)
		return (0);

	htotal = hactive + hblank;
	vtotal = vactive + vblank;
	m->width = hactive;
	m->height = vactive;
	/* milli-Hz: pixclk * 1000 / (htotal*vtotal); 64-bit to avoid overflow. */
	m->refresh_mhz = (uint32_t)(((uint64_t)pixclk * 1000u) /
	    ((uint64_t)htotal * vtotal));
	m->pad = 0;
	return (1);
}

/*
 * MODES: read the EDID over DDC and decode its 4 Detailed Timing Descriptors
 * (at offsets 54/72/90/108) into mode entries — the preferred timing first,
 * matching EDID order. Skips non-timing descriptors. A disconnected connector's
 * EDID is all-0xff → no valid timings → count 0.
 */
static void
display_modes(struct atrium_amd_softc *sc, struct atrium_display_modes *out)
{
	uint8_t edid[ATRIUM_DISPLAY_EDID_LEN];
	struct atrium_display_mode m;
	uint32_t i;
	int off;

	for (i = 0; i < ATRIUM_DISPLAY_EDID_LEN; i++) {
		amd_mmio_write32(sc, regDISP_DDC_OFFSET, i);
		edid[i] = (uint8_t)amd_mmio_read32(sc, regDISP_DDC_DATA);
	}

	out->count = 0;
	out->pad = 0;
	for (off = 54; off + 18 <= ATRIUM_DISPLAY_EDID_LEN; off += 18) {
		if (!display_decode_dtd(&edid[off], &m))
			continue;
		if (out->count < ATRIUM_DISPLAY_MAX_MODES)
			out->modes[out->count] = m;
		out->count++;
	}
}

/*
 * Program FB_BASE/FB_SIZE from an exported scanout {vram_offset, size}. The
 * display reads VRAM by offset, so the base IS the offset — no BO lookup. The
 * referee enforces residency (FbNotResident if no VRAM BO covers it) and bounds
 * (FbTooSmall), surfaced through regDISP_FAULT.
 */
static void
display_program_fb(struct atrium_amd_softc *sc, uint64_t base, uint64_t size)
{
	amd_mmio_write32(sc, regDISP_FB_BASE_LO, (uint32_t)base);
	amd_mmio_write32(sc, regDISP_FB_BASE_HI, (uint32_t)(base >> 32));
	amd_mmio_write32(sc, regDISP_FB_SIZE, (uint32_t)size);
}

/* --- cdev ioctl dispatch -------------------------------------------------- */

static int
display_open(struct cdev *cdev, int oflags, int devtype, struct thread *td)
{
	return (0);
}

static int
display_ioctl(struct cdev *cdev, u_long cmd, caddr_t data, int fflag,
    struct thread *td)
{
	struct atrium_amd_softc *sc = display_softc(cdev);

	switch (cmd) {
	case ATRIUM_DISPLAY_IOC_ENUM: {
		struct atrium_display_connector *c =
		    (struct atrium_display_connector *)data;

		display_enum(sc, c);
		return (0);
	}

	case ATRIUM_DISPLAY_IOC_MODES: {
		struct atrium_display_modes *m =
		    (struct atrium_display_modes *)data;

		display_modes(sc, m);
		return (0);
	}

	case ATRIUM_DISPLAY_IOC_SET_MODE: {
		struct atrium_display_setmode *s =
		    (struct atrium_display_setmode *)data;

		display_program_fb(sc, s->vram_offset, s->size);
		amd_mmio_write32(sc, regDISP_SET_MODE, 1);
		s->fault = amd_mmio_read32(sc, regDISP_FAULT);
		return (0);
	}

	case ATRIUM_DISPLAY_IOC_PAGE_FLIP: {
		struct atrium_display_flip *f =
		    (struct atrium_display_flip *)data;

		display_program_fb(sc, f->vram_offset, f->size);
		amd_mmio_write32(sc, regDISP_FLIP, f->vsync & 1u);
		f->fault = amd_mmio_read32(sc, regDISP_FAULT);
		return (0);
	}

	case ATRIUM_DISPLAY_IOC_STATUS: {
		struct atrium_display_status *st =
		    (struct atrium_display_status *)data;

		st->vblank_count = amd_mmio_read32(sc, regDISP_VBLANK_COUNT);
		st->dropped_flips = amd_mmio_read32(sc, regDISP_DROPPED_FLIPS);
		st->tear_line = amd_mmio_read32(sc, regDISP_TEAR_LINE);
		return (0);
	}

	case ATRIUM_DISPLAY_IOC_CONFIG: {
		struct atrium_display_config *c =
		    (struct atrium_display_config *)data;

		amd_mmio_write32(sc, regDISP_CFG_CONNECTOR_TYPE, c->connector_type);
		amd_mmio_write32(sc, regDISP_CFG_PLUG_MODE, c->plug_mode);
		return (0);
	}

	case ATRIUM_DISPLAY_IOC_USBC: {
		struct atrium_display_usbc *u =
		    (struct atrium_display_usbc *)data;

		amd_mmio_write32(sc, regDISP_CFG_USBC, u->lanes);
		return (0);
	}

	case ATRIUM_DISPLAY_IOC_MST: {
		struct atrium_display_mst *m = (struct atrium_display_mst *)data;

		switch (m->op) {
		case 0:	/* enable / reset the hub */
			amd_mmio_write32(sc, regDISP_MST_ENABLE, 1);
			break;
		case 1:	/* add a sink advertising mode `arg` */
			amd_mmio_write32(sc, regDISP_MST_ADD_SINK, m->arg);
			break;
		case 2:	/* query sink `arg` */
			amd_mmio_write32(sc, regDISP_MST_SELECT, m->arg);
			m->starved = amd_mmio_read32(sc, regDISP_MST_SINK_STARVED);
			break;
		}
		m->count = amd_mmio_read32(sc, regDISP_MST_SINK_COUNT);
		return (0);
	}

	case ATRIUM_DISPLAY_IOC_DPTRAIN: {
		struct atrium_display_dptrain *t =
		    (struct atrium_display_dptrain *)data;

		amd_mmio_write32(sc, regDISP_DPTRAIN_CABLE_RATE, t->cable_rate);
		amd_mmio_write32(sc, regDISP_DPTRAIN_CABLE_LANES, t->cable_lanes);
		amd_mmio_write32(sc, regDISP_DPTRAIN_RUN, 1);
		t->bw_mbps = amd_mmio_read32(sc, regDISP_DPTRAIN_BW_MBPS);
		t->trained = amd_mmio_read32(sc, regDISP_DPTRAIN_TRAINED);
		return (0);
	}

	default:
		return (ENOTTY);
	}
}

static struct cdevsw atrium_display_cdevsw = {
	.d_version =	D_VERSION,
	.d_name =	"atrium-display",
	.d_open =	display_open,
	.d_ioctl =	display_ioctl,
};

/* --- newbus child driver -------------------------------------------------- */

static int
atrium_display_probe(device_t dev)
{
	const char *name = device_get_name(dev);

	/* The base module names our child "atrium_gpu_amd_display"; claim it. */
	if (name != NULL && strcmp(name, "atrium_gpu_amd_display") == 0) {
		device_set_desc(dev, "Atrium AMD display engine (gpusim)");
		return (BUS_PROBE_DEFAULT);
	}
	return (ENXIO);
}

static int
atrium_display_attach(device_t dev)
{
	struct atrium_amd_softc *sc = device_get_softc(device_get_parent(dev));

	/*
	 * Share the base module's softc (BARs already mapped, sc->dev set). The
	 * display only needs the register file; no DMA, no MSI-X of its own — the
	 * live-tier vblank is pulsed by the QEMU device, and STATUS polls the
	 * model's counters. Publish the cdev.
	 */
	sc->display_cdev = make_dev(&atrium_display_cdevsw, device_get_unit(dev),
	    UID_ROOT, GID_WHEEL, 0600, "atrium-display%d", device_get_unit(dev));
	if (sc->display_cdev == NULL) {
		device_printf(dev, "failed to create /dev/atrium-display%d\n",
		    device_get_unit(dev));
		return (ENXIO);
	}
	sc->display_cdev->si_drv1 = sc;

	device_printf(dev, "ready: /dev/atrium-display%d\n", device_get_unit(dev));
	return (0);
}

static int
atrium_display_detach(device_t dev)
{
	struct atrium_amd_softc *sc = device_get_softc(device_get_parent(dev));

	if (sc->display_cdev != NULL) {
		destroy_dev(sc->display_cdev);
		sc->display_cdev = NULL;
	}
	return (0);
}

static device_method_t atrium_display_methods[] = {
	DEVMETHOD(device_probe,		atrium_display_probe),
	DEVMETHOD(device_attach,	atrium_display_attach),
	DEVMETHOD(device_detach,	atrium_display_detach),
	DEVMETHOD_END
};

static driver_t atrium_display_driver = {
	"atrium_gpu_amd_display",
	atrium_display_methods,
	0,	/* no per-child softc: state lives in the base's SHARED softc, reached
		 * via device_get_softc(device_get_parent(dev)) — never this child's. */
};

/* Attach to the "atrium_gpu_amd_display" child of the base (pci) module. */
DRIVER_MODULE(atrium_gpu_amd_display, atrium_gpu_amd_pci, atrium_display_driver,
    NULL, NULL);
MODULE_DEPEND(atrium_gpu_amd_display, atrium_gpu_amd_pci, 1, 1, 1);
MODULE_VERSION(atrium_gpu_amd_display, 1);
