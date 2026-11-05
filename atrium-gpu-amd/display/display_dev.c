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
 * over DDC, decode its detailed timing, scan out a VRAM BO, flip (vsync on =
 * latch at vblank, off = tear), and read back vblank / tear / fault status.
 *
 * Scanout is by object (display ABI v2, include/atrium/display_abi_v2.h): a
 * commit names the BO by its fd. This module resolves the fd through the gpu
 * module's exported vtable (sc->backend; it links against nothing there),
 * holds a file reference on the BO while it may be on screen, waits on the
 * commit's in-fence before flipping and signals its out-fence when the flip
 * lands (IH cause FLIP). Userspace never supplies an address. The node is
 * exclusive, and what is on screen stays there when its holder closes.
 */
#include "atrium_gpu_amd.h"		/* shared softc + mmio accessors + regDISP_* */
#include "atrium_display_abi.h"
#include <atrium/display_abi_v2.h>

#include <sys/module.h>
#include <sys/kernel.h>
#include <sys/jail.h>
#include <sys/limits.h>
#include <sys/sysctl.h>
#include <sys/ucred.h>
#include "energy_compat.h"	/* the display's energy-federation member */
#include "display_hw.h"		/* atrium-display/: shared EDID + connector code */

#include <sys/malloc.h>

/*
 * The display module's own connector state (display ABI 2.1), hung off the
 * shared softc as sc->display_state. Guarded by sc->lock.
 *   sink         the monitor as last probed (raw EDID and its modes)
 *   sink_gen     sc->display_conn_gen when it was probed (0 = never)
 *   timing       the timing of the lit mode, re-applied after a device reset
 */
struct atrium_amd_display_state {
	struct atrium_display_sink	sink;
	uint32_t			sink_gen;
	struct atrium_display_timing	timing;
	int				timing_valid;
};

static MALLOC_DEFINE(M_ATRIUM_DISPLAY, "atrium_display", "Atrium display state");

/* KNOTE hints for events (any real vblank hint is a small count). */
#define DISPLAY_HINT_RESET	LONG_MIN
#define DISPLAY_HINT_HOTPLUG	(LONG_MIN + 1)

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
 * --- the AMD (DCN 4) side of atrium_display_hw_ops ---------------------------
 *
 * Everything below the generic connector code (atrium-display/display_hw.h):
 * how this hardware senses a monitor (HPD0), reads its EDID (DDC; the SIM
 * byte port until D5 brings up the real I2C engine) and programs a timing
 * (OTG0's timing registers, the pixel clock on a SIM register until DCCG is
 * modelled).
 */
static int
amd_disp_sense(void *hw)
{
	struct atrium_amd_softc *sc = hw;

	return ((amd_mmio_read32(sc, regHPD0_DC_HPD_INT_STATUS) &
	    DCN_HPD_SENSE) != 0);
}

static int
amd_disp_read_edid_block(void *hw, uint32_t n, uint8_t *out)
{
	struct atrium_amd_softc *sc = hw;
	uint32_t i;

	for (i = 0; i < ATRIUM_EDID_BLOCK; i++) {
		amd_mmio_write32(sc, regDISP_DDC_OFFSET, n * ATRIUM_EDID_BLOCK + i);
		out[i] = (uint8_t)amd_mmio_read32(sc, regDISP_DDC_DATA);
	}
	return (0);
}

/*
 * OTG timing, with the line starting at the leading edge of sync: TOTAL is
 * total - 1, SYNC_A runs from 0 to the sync width, BLANK_END is the first
 * active pixel (sync + back porch) and BLANK_START the first blank one.
 */
static void
amd_disp_program_timing(void *hw, const struct atrium_display_timing *t)
{
	struct atrium_amd_softc *sc = hw;
	uint32_t hb = t->hsync + t->hback, vb = t->vsync + t->vback;

	amd_mmio_write32(sc, regOTG0_OTG_H_TOTAL,
	    t->hactive + t->hfront + t->hsync + t->hback - 1);
	amd_mmio_write32(sc, regOTG0_OTG_H_BLANK_START_END,
	    (hb + t->hactive) | (hb << DCN_OTG_END_SHIFT));
	amd_mmio_write32(sc, regOTG0_OTG_H_SYNC_A,
	    t->hsync << DCN_OTG_END_SHIFT);
	amd_mmio_write32(sc, regOTG0_OTG_V_TOTAL,
	    t->vactive + t->vfront + t->vsync + t->vback - 1);
	amd_mmio_write32(sc, regOTG0_OTG_V_BLANK_START_END,
	    (vb + t->vactive) | (vb << DCN_OTG_END_SHIFT));
	amd_mmio_write32(sc, regOTG0_OTG_V_SYNC_A,
	    t->vsync << DCN_OTG_END_SHIFT);
	amd_mmio_write32(sc, regDISP_PIXEL_CLOCK_KHZ, t->pixel_clock_khz);
}

/*
 * HPD0 is a level interrupt that fires while SENSE equals POLARITY: wait for
 * the opposite of what is there now (a disconnect while a monitor is
 * present, a connect while none is), then acknowledge.
 */
static void
amd_disp_hpd_rearm(void *hw)
{
	struct atrium_amd_softc *sc = hw;
	int present;

	amd_mmio_write32(sc, regHPD0_DC_HPD_CONTROL, DCN_HPD_EN);
	present = amd_disp_sense(sc);
	amd_mmio_write32(sc, regHPD0_DC_HPD_INT_CONTROL, DCN_HPD_INT_EN |
	    (present ? 0 : DCN_HPD_INT_POLARITY) | DCN_HPD_INT_ACK);
}

static const struct atrium_display_hw_ops amd_display_hw_ops = {
	.sense =		amd_disp_sense,
	.read_edid_block =	amd_disp_read_edid_block,
	.program_timing =	amd_disp_program_timing,
	.hpd_rearm =		amd_disp_hpd_rearm,
};

/*
 * The monitor as of the current hotplug generation, into `out`; returns that
 * generation. Probing reads up to four EDID blocks, so it runs unlocked, and
 * only when a hotplug has happened since the last probe.
 */
static uint32_t
display_sink_get(struct atrium_amd_softc *sc, struct atrium_display_sink *out)
{
	struct atrium_amd_display_state *ds = sc->display_state;
	uint32_t gen;

	mtx_lock(&sc->lock);
	gen = sc->display_conn_gen;
	if (ds->sink_gen == gen) {
		*out = ds->sink;
		mtx_unlock(&sc->lock);
		return (gen);
	}
	mtx_unlock(&sc->lock);
	(void)atrium_display_probe_sink(&amd_display_hw_ops, sc, out);
	mtx_lock(&sc->lock);
	if (sc->display_conn_gen == gen) {
		ds->sink = *out;
		ds->sink_gen = gen;
	}
	mtx_unlock(&sc->lock);
	return (gen);
}

/*
 * Interim ENUM: HPD, the §8 type and the EDID base block. A disconnected
 * connector reads all 0xff, so the caller's parse fails its header (the
 * realistic "unplugged" failure mode).
 */
static void
display_enum(struct atrium_amd_softc *sc, struct atrium_display_connector *c)
{
	struct atrium_display_sink *sink;

	sink = malloc(sizeof(*sink), M_ATRIUM_DISPLAY, M_WAITOK);
	(void)display_sink_get(sc, sink);
	c->connected = sink->connected;
	c->connector_type = amd_mmio_read32(sc, regDISP_CONNECTOR_TYPE);
	c->usbc_lanes = amd_mmio_read32(sc, regDISP_USBC_LANES);
	memset(c->edid, 0xff, sizeof(c->edid));
	if (sink->edid_len >= ATRIUM_DISPLAY_EDID_LEN)
		memcpy(c->edid, sink->edid, ATRIUM_DISPLAY_EDID_LEN);
	c->edid_len = ATRIUM_DISPLAY_EDID_LEN;
	free(sink, M_ATRIUM_DISPLAY);
}

static void
display_mode_v2(const struct atrium_display_timing *t,
    struct atrium_display_mode_v2 *m)
{
	m->width = t->hactive;
	m->height = t->vactive;
	m->refresh_mhz = atrium_timing_refresh_mhz(t);
	m->pixel_clock_khz = t->pixel_clock_khz;
	m->flags = (t->flags & ATRIUM_TIMING_PREFERRED) ?
	    ATRIUM_DISPLAY_MODE_PREFERRED : 0;
	m->_pad = 0;
}

/* Interim MODES: the first ATRIUM_DISPLAY_MAX_MODES of the connector's. */
static void
display_modes(struct atrium_amd_softc *sc, struct atrium_display_modes *out)
{
	struct atrium_display_sink *sink;
	struct atrium_display_mode_v2 m;
	uint32_t i;

	sink = malloc(sizeof(*sink), M_ATRIUM_DISPLAY, M_WAITOK);
	(void)display_sink_get(sc, sink);
	out->count = sink->ntimings;
	out->pad = 0;
	for (i = 0; i < sink->ntimings && i < ATRIUM_DISPLAY_MAX_MODES; i++) {
		display_mode_v2(&sink->timings[i], &m);
		out->modes[i].width = m.width;
		out->modes[i].height = m.height;
		out->modes[i].refresh_mhz = m.refresh_mhz;
		out->modes[i].pad = 0;
	}
	free(sink, M_ATRIUM_DISPLAY);
}

/* CONNECTOR (2.1): the monitor, its raw EDID and every mode it offers. */
static int
display_connector(struct atrium_amd_softc *sc,
    struct atrium_display_connector_v2 *c)
{
	struct atrium_amd_display_state *ds = sc->display_state;
	struct atrium_display_sink *sink;
	uint32_t i, n;

	if (c->struct_size < sizeof(*c))
		return (EINVAL);
	sink = malloc(sizeof(*sink), M_ATRIUM_DISPLAY, M_WAITOK);
	c->out_generation = display_sink_get(sc, sink);
	c->out_flags = sink->connected ? ATRIUM_DISPLAY_CONNECTOR_CONNECTED : 0;
	c->out_connector_type = amd_mmio_read32(sc, regDISP_CONNECTOR_TYPE);
	n = sink->edid_len < ATRIUM_DISPLAY_EDID_MAX ? sink->edid_len :
	    ATRIUM_DISPLAY_EDID_MAX;
	memset(c->out_edid, 0, sizeof(c->out_edid));
	memcpy(c->out_edid, sink->edid, n);
	c->out_edid_len = n;
	c->out_mode_count = 0;
	c->out_current_mode = 0xffffffffu;
	c->_pad = 0;
	memset(c->out_modes, 0, sizeof(c->out_modes));
	mtx_lock(&sc->lock);
	for (i = 0; i < sink->ntimings && i < ATRIUM_DISPLAY_CONNECTOR_MODES;
	    i++) {
		display_mode_v2(&sink->timings[i], &c->out_modes[i]);
		if (sc->display_mode_set && ds->timing_valid &&
		    atrium_timing_equal(&ds->timing, &sink->timings[i]))
			c->out_current_mode = i;
		c->out_mode_count++;
	}
	mtx_unlock(&sc->lock);
	free(sink, M_ATRIUM_DISPLAY);
	return (0);
}

/* --- cdev ioctl dispatch -------------------------------------------------- */

/* --- commit state machine (display ABI v2) ------------------------------- */

/*
 * The display keeps a file reference on each BO it may be scanning out, and on
 * each fence it still has to wait on or signal. The interrupt path runs with
 * sc->lock held and can't fdrop, so it parks references here; display_drain
 * drops them later in process context.
 */
static void
display_park(struct atrium_amd_softc *sc, struct file *fp)
{
	mtx_assert(&sc->lock, MA_OWNED);
	if (fp == NULL)
		return;
	/* At most one commit is outstanding and every ioctl drains, so a landing
	 * parks at most three references before the next drain. */
	KASSERT(sc->display_nrelease < ATRIUM_AMD_DISPLAY_NRELEASE,
	    ("atrium display: release list full"));
	sc->display_release[sc->display_nrelease++] = fp;
}

static void
display_drain(struct atrium_amd_softc *sc, struct thread *td)
{
	struct file *fps[ATRIUM_AMD_DISPLAY_NRELEASE];
	int i, n;

	mtx_lock(&sc->lock);
	n = sc->display_nrelease;
	for (i = 0; i < n; i++) {
		fps[i] = sc->display_release[i];
		sc->display_release[i] = NULL;
	}
	sc->display_nrelease = 0;
	mtx_unlock(&sc->lock);
	for (i = 0; i < n; i++)
		fdrop(fps[i], td);
}

/*
 * Program HUBP0 to scan out `fb`, under SURFACE_UPDATE_LOCK: the address write
 * that would otherwise request a flip is held until display_unlock(). The
 * surface address is the BO's VRAM offset (the display reads VRAM by offset;
 * the referee checks residency and bounds and reports through regDISP_FAULT).
 * XRGB8888 linear is DCN's 32 bpp ARGB8888, linear swizzle, alpha plane off.
 */
static void
display_program_surface(struct atrium_amd_softc *sc,
    const struct atrium_amd_display_fb *fb, int vsync)
{
	uint32_t pitch = fb->pitch != 0 ? fb->pitch : sc->display_width * 4;
	uint32_t type = vsync ? 0 : DCN_SURFACE_FLIP_TYPE;

	amd_mmio_write32(sc, regHUBPREQ0_DCSURF_FLIP_CONTROL,
	    DCN_SURFACE_UPDATE_LOCK | type);
	amd_mmio_write32(sc, regHUBP0_DCSURF_SURFACE_CONFIG,
	    DCN_PIXEL_FORMAT_ARGB8888);
	amd_mmio_write32(sc, regHUBP0_DCSURF_TILING_CONFIG, DCN_SW_MODE_LINEAR);
	amd_mmio_write32(sc, regHUBPREQ0_DCSURF_SURFACE_PITCH,
	    pitch >= 4 ? (pitch / 4 - 1) & DCN_PITCH_MASK : 0);
	amd_mmio_write32(sc, regHUBPREQ0_DCSURF_PRIMARY_SURFACE_ADDRESS_HIGH,
	    (uint32_t)(fb->vram_offset >> 32) & DCN_ADDRESS_HIGH_MASK);
	amd_mmio_write32(sc, regHUBPREQ0_DCSURF_PRIMARY_SURFACE_ADDRESS,
	    (uint32_t)fb->vram_offset);
}

/* Release SURFACE_UPDATE_LOCK: a held surface update becomes a flip, at
 * vsync or at once as display_program_surface chose. */
static void
display_unlock(struct atrium_amd_softc *sc, int vsync)
{
	amd_mmio_write32(sc, regHUBPREQ0_DCSURF_FLIP_CONTROL,
	    vsync ? 0 : DCN_SURFACE_FLIP_TYPE);
}

/* Arm (or disarm) OTG0's vertical interrupt 0, our per-frame interrupt, and
 * HUBP0's flip interrupt; both acknowledge anything latched. */
static void
display_irqs(struct atrium_amd_softc *sc, int vblank, int flip)
{
	amd_mmio_write32(sc, regOTG0_OTG_VERTICAL_INTERRUPT0_CONTROL,
	    DCN_VINT0_CLEAR | (vblank ? DCN_VINT0_INT_ENABLE : 0));
	amd_mmio_write32(sc, regHUBPREQ0_DCSURF_SURFACE_FLIP_INTERRUPT,
	    DCN_SURFACE_FLIP_CLEAR | (flip ? DCN_SURFACE_FLIP_INT_MASK : 0));
}

/* Frames since attach: OTG_STATUS_FRAME_COUNT is 24 bits, widened here. */
static uint64_t
display_frames(struct atrium_amd_softc *sc)
{
	uint32_t raw = amd_mmio_read32(sc, regOTG0_OTG_STATUS_FRAME_COUNT) &
	    DCN_OTG_FRAME_COUNT_MASK;

	sc->display_frames += (raw - sc->display_frame_raw) &
	    DCN_OTG_FRAME_COUNT_MASK;
	sc->display_frame_raw = raw;
	return (sc->display_frames);
}

/*
 * The OTG frame counter restarts when the timing generator is (re)enabled: on
 * a mode set and across an FLR. Bank the frames counted so far, then take the
 * restarted value as the new base, so the counter userland sees stays
 * monotonic and does not jump by 2^24 at each restart.
 */
static void
display_frames_rebase(struct atrium_amd_softc *sc)
{
	sc->display_frame_raw = amd_mmio_read32(sc,
	    regOTG0_OTG_STATUS_FRAME_COUNT) & DCN_OTG_FRAME_COUNT_MASK;
}

static void
display_set_mode(struct atrium_amd_softc *sc)
{
	(void)display_frames(sc);
	amd_mmio_write32(sc, regDISP_SET_MODE, 1);
	display_frames_rebase(sc);
}

/* Signal (and let go of) the outstanding commit's out-fence. */
static void
display_signal_out(struct atrium_amd_softc *sc)
{
	mtx_assert(&sc->lock, MA_OWNED);
	if (sc->display_out_so != NULL && sc->backend != NULL)
		sc->backend->syncobj_signal(sc->display_out_so,
		    sc->display_out_value);
	display_park(sc, sc->display_out_fp);
	sc->display_out_fp = NULL;
	sc->display_out_so = NULL;
}

static void
display_drop_in(struct atrium_amd_softc *sc)
{
	if (sc->display_in_so != NULL && sc->backend != NULL)
		sc->backend->syncobj_watch(sc->display_in_so, NULL, NULL);
	display_park(sc, sc->display_in_fp);
	sc->display_in_fp = NULL;
	sc->display_in_so = NULL;
}

/* The outstanding commit is on screen: it replaces the old on-screen BO, which
 * is released, and the out-fence fires. */
static void
display_landed(struct atrium_amd_softc *sc)
{
	mtx_assert(&sc->lock, MA_OWNED);
	display_park(sc, sc->display_on_screen.bo_fp);
	sc->display_on_screen = sc->display_next;
	sc->display_flips_landed++;
	/* A frame rendered after the last reset repaints the screen. */
	if (sc->display_on_screen.reset_gen == sc->reset_gen)
		sc->display_content_lost = 0;
	memset(&sc->display_next, 0, sizeof(sc->display_next));
	sc->display_next_state = 0;
	display_drop_in(sc);
	display_signal_out(sc);
}

/* The outstanding commit won't reach the screen (the hardware refused it, or
 * its holder went away before its fence): drop it, keep the old buffer up,
 * and still signal the out-fence so nothing waits forever. */
static void
display_abandon(struct atrium_amd_softc *sc)
{
	mtx_assert(&sc->lock, MA_OWNED);
	display_park(sc, sc->display_next.bo_fp);
	memset(&sc->display_next, 0, sizeof(sc->display_next));
	sc->display_next_state = 0;
	display_drop_in(sc);
	display_signal_out(sc);
}

/* Has the flip we programmed landed? HUBP0 holds SURFACE_FLIP_PENDING until
 * the requested surface is the one being scanned out. */
static void
display_reap(struct atrium_amd_softc *sc)
{
	mtx_assert(&sc->lock, MA_OWNED);
	if (sc->display_next_state != 2)
		return;
	if ((amd_mmio_read32(sc, regHUBPREQ0_DCSURF_FLIP_CONTROL) &
	    DCN_SURFACE_FLIP_PENDING) == 0)
		display_landed(sc);
}

/* Program the outstanding commit's flip. Returns the referee fault (0 = ok);
 * on a fault nothing was queued. */
static uint32_t
display_flip_next(struct atrium_amd_softc *sc)
{
	uint32_t fault;

	mtx_assert(&sc->lock, MA_OWNED);
	/* Lock, program, unlock: the unlock is the flip. */
	display_program_surface(sc, &sc->display_next, sc->display_next_vsync);
	display_unlock(sc, sc->display_next_vsync);
	fault = amd_mmio_read32(sc, regDISP_FAULT);
	if (fault != 0)
		return (fault);
	sc->display_next_state = 2;
	display_drop_in(sc);	/* the fence has done its job */
	display_reap(sc);	/* an immediate flip has already landed */
	return (0);
}

static int
display_in_fence_ready(struct atrium_amd_softc *sc)
{
	struct atrium_amd_syncobj *so = sc->display_in_so;
	int ready;

	if (so == NULL)
		return (1);
	mtx_lock(&so->lock);
	if (so->value >= sc->display_in_value)
		ready = 1;
	else if (so->error != 0)
		ready = -so->error;	/* the render will never finish */
	else
		ready = 0;
	mtx_unlock(&so->lock);
	return (ready);
}

/*
 * A commit waiting on its in-fence flips as soon as the fence signals: the
 * syncobj's watch (display_fence_kick) queues display_fence_task, which runs
 * this. Vblank and the next commit retry it too, as a backstop. An in-fence
 * that fails (its render was lost to a hang, fault or reset) drops the
 * commit: the old buffer stays up and the out-fence fails with the same
 * error, so the holder knows the frame never showed (GPU ABI v2.1 §10.3).
 */
static void
display_try_fence(struct atrium_amd_softc *sc)
{
	uint32_t fault;
	int ready;

	mtx_assert(&sc->lock, MA_OWNED);
	if (sc->display_next_state != 1)
		return;
	ready = display_in_fence_ready(sc);
	if (ready < 0) {
		if (sc->display_out_so != NULL && sc->backend != NULL)
			sc->backend->syncobj_fail(sc->display_out_so, -ready);
		display_park(sc, sc->display_out_fp);
		sc->display_out_fp = NULL;
		sc->display_out_so = NULL;
		display_abandon(sc);
		return;
	}
	if (ready == 0)
		return;
	fault = display_flip_next(sc);
	if (fault != 0) {
		sc->display_last_fault = fault;
		display_abandon(sc);
	}
}

/* The in-fence's watch: called under the syncobj's lock, so only queue. */
static void
display_fence_kick(void *arg)
{
	struct atrium_amd_softc *sc = arg;

	taskqueue_enqueue(taskqueue_thread, &sc->display_fence_task);
}

static void
display_fence_task(void *arg, int pending __unused)
{
	struct atrium_amd_softc *sc = arg;

	mtx_lock(&sc->lock);
	display_try_fence(sc);
	mtx_unlock(&sc->lock);
	display_drain(sc, curthread);
}

/* IH FLIP cause: a flip reached the screen. sc->lock held. */
static void
amd_display_flip_handler(struct atrium_amd_softc *sc, int count __unused)
{
	/* Acknowledge (FLIP_CLEAR), keeping the interrupt enabled. */
	amd_mmio_write32(sc, regHUBPREQ0_DCSURF_SURFACE_FLIP_INTERRUPT,
	    DCN_SURFACE_FLIP_CLEAR | DCN_SURFACE_FLIP_INT_MASK);
	display_reap(sc);
}

/*
 * Does a scanout buffer cover a width x height XRGB8888 mode? The pitch must
 * hold a full row and pitch x height must fit in the BO, or the display engine
 * would read past the BO into whatever VRAM follows it (isolation break 4).
 */
static int
display_fb_fits(const struct atrium_amd_display_fb *fb, uint32_t width,
    uint32_t height)
{
	uint64_t pitch = fb->pitch != 0 ? fb->pitch : (uint64_t)width * 4;

	/* pitch 0 means packed (width * 4), as the ABI defines it. */
	if (pitch < (uint64_t)width * 4)
		return (0);
	return (pitch * height <= fb->size);
}

static int
display_commit(struct atrium_amd_softc *sc, struct atrium_display_commit *c,
    struct thread *td)
{
	const struct atrium_gpu_backend_ops *ops;
	struct atrium_amd_display_fb fb;
	struct atrium_amd_bo *bo;
	struct atrium_amd_syncobj *in_so = NULL, *out_so = NULL;
	struct file *bo_fp = NULL, *in_fp = NULL, *out_fp = NULL;
	struct atrium_amd_display_state *ds = sc->display_state;
	struct atrium_display_sink *sink = NULL;
	struct atrium_display_timing tm = { 0 };
	uint64_t off, size;
	uint32_t fault, gen;
	int err, have_tm = 0;

	c->out_fault = 0;
	if (c->struct_size < sizeof(*c) ||
	    (c->flags & ~(ATRIUM_DISPLAY_COMMIT_MODESET |
	    ATRIUM_DISPLAY_COMMIT_ASYNC | ATRIUM_DISPLAY_COMMIT_TEST_ONLY)) != 0 ||
	    c->format != ATRIUM_DISPLAY_FORMAT_XRGB8888 ||
	    c->modifier != ATRIUM_DISPLAY_MODIFIER_LINEAR || c->bo_offset != 0 ||
	    (c->pitch % 256) != 0)
		return (EINVAL);

	display_drain(sc, td);

	mtx_lock(&sc->lock);
	ops = sc->backend;
	mtx_unlock(&sc->lock);
	if (ops == NULL || ops->bo_fget == NULL)
		return (ENXIO);

	err = ops->bo_fget(td, c->bo_fd, &bo_fp, &bo);
	if (err != 0)
		return (err);
	err = ops->export_scanout(bo, &off, &size);	/* VRAM only */
	if (err != 0)
		goto out;
	if (c->in_fence_fd >= 0 &&
	    (err = ops->syncobj_fget(td, c->in_fence_fd, &in_fp, &in_so)) != 0)
		goto out;
	if (c->out_fence_fd >= 0 &&
	    (err = ops->syncobj_fget(td, c->out_fence_fd, &out_fp, &out_so)) != 0)
		goto out;
	if (c->flags & ATRIUM_DISPLAY_COMMIT_TEST_ONLY)
		goto out;	/* the fds resolve and the BO is scannable */

	fb.bo_fp = bo_fp;
	fb.reset_gen = sc->reset_gen;	/* unlocked read: a racing reset only
					 * makes this frame count as stale */
	fb.vram_offset = off;
	fb.size = size;
	fb.pitch = c->pitch;
	fb.format = c->format;
	if (c->flags & ATRIUM_DISPLAY_COMMIT_MODESET) {
		/* The mode comes from the connector's list (probed unlocked). */
		sink = malloc(sizeof(*sink), M_ATRIUM_DISPLAY, M_WAITOK);
		gen = display_sink_get(sc, sink);
		if (c->mode_generation != 0 && c->mode_generation != gen) {
			err = ESTALE;	/* the monitor changed since CONNECTOR */
			goto out;
		}
		if (sink->ntimings > 0) {
			if (c->mode_index >= sink->ntimings) {
				err = EINVAL;
				goto out;
			}
			tm = sink->timings[c->mode_index];
			have_tm = 1;
			/* The kernel checks the buffer covers the mode it will
			 * scan out; gpusim's referee is the second line, not
			 * the first (review §A2.5, isolation break 4). */
			if (!display_fb_fits(&fb, tm.hactive, tm.vactive)) {
				err = EINVAL;
				goto out;
			}
		} else if (c->mode_index != 0) {
			err = EINVAL;	/* no monitor: nothing to index */
			goto out;
		}
	}

	mtx_lock(&sc->lock);
	display_reap(sc);
	if (sc->display_next_state != 0) {
		mtx_unlock(&sc->lock);
		err = EBUSY;
		goto out;
	}

	if (c->flags & ATRIUM_DISPLAY_COMMIT_MODESET) {
		sc->display_in_so = in_so;
		if ((err = display_in_fence_ready(sc)) <= 0) {
			sc->display_in_so = NULL;
			mtx_unlock(&sc->lock);
			err = err < 0 ? -err : EAGAIN;	/* failed render: its error */
			goto out;
		}
		sc->display_in_so = NULL;
		if (have_tm) {
			sc->display_width = tm.hactive;
			amd_display_hw_ops.program_timing(sc, &tm);
		}
		/* The surface goes up with the mode (the SIM mode set takes the
		 * held surface); the unlock after it then has nothing to flip. */
		display_program_surface(sc, &fb, 1);
		display_set_mode(sc);
		display_unlock(sc, 1);
		fault = amd_mmio_read32(sc, regDISP_FAULT);
		if (fault != 0) {
			mtx_unlock(&sc->lock);
			c->out_fault = fault;
			err = 0;	/* a referee fault: reported in out_fault */
			goto out;
		}
		/*
		 * The CRTC is live: arm the vblank and flip-done interrupts. The
		 * device raises them through the IH ring and the base ISR routes
		 * them here (interrupt-driven like real silicon).
		 */
		display_irqs(sc, 1, 1);
		sc->display_vblank_armed = 1;	/* remembered across a device reset */
		sc->display_mode_set = 1;
		ds->timing_valid = have_tm;
		if (have_tm)
			ds->timing = tm;
		sc->display_refresh_mhz = have_tm ?
		    atrium_timing_refresh_mhz(&tm) : 0;
		sc->display_mode_width = have_tm ? tm.hactive : 0;
		sc->display_mode_height = have_tm ? tm.vactive : 0;
		/* A modeset scans the new buffer out from its first frame. */
		display_park(sc, sc->display_on_screen.bo_fp);
		sc->display_on_screen = fb;
		if (out_so != NULL)
			ops->syncobj_signal(out_so, c->out_fence_value);
		display_park(sc, out_fp);
		display_park(sc, in_fp);
		mtx_unlock(&sc->lock);
		display_drain(sc, td);
		free(sink, M_ATRIUM_DISPLAY);
		return (0);
	}

	if (sc->display_mode_set && sc->display_mode_width != 0 &&
	    !display_fb_fits(&fb, sc->display_mode_width,
	    sc->display_mode_height)) {
		mtx_unlock(&sc->lock);
		err = EINVAL;	/* would scan out past the end of the BO */
		goto out;
	}
	if (!sc->display_mode_set) {
		mtx_unlock(&sc->lock);
		c->out_fault = ATRIUM_DISPLAY_FAULT_NO_MODE;
		err = 0;
		goto out;
	}
	sc->display_next = fb;
	sc->display_next_vsync = (c->flags & ATRIUM_DISPLAY_COMMIT_ASYNC) == 0;
	sc->display_in_fp = in_fp;
	sc->display_in_so = in_so;
	sc->display_in_value = c->in_fence_value;
	sc->display_out_fp = out_fp;
	sc->display_out_so = out_so;
	sc->display_out_value = c->out_fence_value;
	sc->display_next_state = 1;
	/* Flip the moment the render signals (display_fence_kick), installed
	 * before the check so a signal in between is not missed. */
	if (in_so != NULL)
		ops->syncobj_watch(in_so, display_fence_kick, sc);
	if (display_in_fence_ready(sc) > 0) {
		fault = display_flip_next(sc);
		if (fault != 0) {
			/* Refused now: report it to the caller, keep its references
			 * for it to release, and leave the screen as it was. */
			memset(&sc->display_next, 0, sizeof(sc->display_next));
			sc->display_next_state = 0;
			if (in_so != NULL)
				ops->syncobj_watch(in_so, NULL, NULL);
			sc->display_in_fp = NULL;
			sc->display_in_so = NULL;
			sc->display_out_fp = NULL;
			sc->display_out_so = NULL;
			mtx_unlock(&sc->lock);
			c->out_fault = fault;
			err = 0;
			goto out;
		}
	} else
		display_try_fence(sc);	/* a failed in-fence drops it now */
	mtx_unlock(&sc->lock);
	display_drain(sc, td);
	return (0);	/* the display owns the references now */

out:
	if (out_fp != NULL)
		fdrop(out_fp, td);
	if (in_fp != NULL)
		fdrop(in_fp, td);
	fdrop(bo_fp, td);
	free(sink, M_ATRIUM_DISPLAY);	/* free(NULL) is a no-op */
	return (err);
}

/*
 * May this credential open the display? The node's permissions decide who can
 * reach it at all (devfs: 0600 root on the host, visible only in frescod's
 * jail); the lease then names the one jail and uid that may hold it, so no
 * other process, root on the host included, can grab the screen while frescod
 * restarts. Without a lease only host processes may open it (bring-up and the
 * display tests); a jailed opener needs a lease naming its jail.
 */
static int
display_lease_check(struct atrium_amd_softc *sc, struct ucred *cr)
{
	char jail[sizeof(sc->display_lease_jail)];
	struct prison *pr = cr->cr_prison;
	uid_t uid;
	int set, match;

	mtx_lock(&sc->lock);
	set = sc->display_lease_set;
	uid = sc->display_lease_uid;
	strlcpy(jail, sc->display_lease_jail, sizeof(jail));
	mtx_unlock(&sc->lock);

	if (!set)
		return (pr == &prison0 ? 0 : EPERM);
	if (pr == &prison0 || cr->cr_uid != uid)
		return (EPERM);
	prison_lock(pr);
	match = strcmp(pr->pr_name, jail) == 0;
	prison_unlock(pr);
	return (match ? 0 : EPERM);
}

/*
 * dev.atrium_gpu_amd_display.N.sim_plug: plug a simulated monitor (the
 * CONFIG plug_mode codes; -1 unplugs) from outside the lease. A real cable
 * is plugged by someone other than the display's holder, so the hotplug
 * path is tested the same way: frescod keeps the display open while the
 * host swaps the monitor under it. Host root only (no CTLFLAG_PRISON), and
 * it drives the simulator's SIM registers, which silicon does not have.
 */
static int
display_sim_plug_sysctl(SYSCTL_HANDLER_ARGS)
{
	struct atrium_amd_softc *sc = arg1;
	int mode = 0, error;

	error = sysctl_handle_int(oidp, &mode, 0, req);
	if (error != 0 || req->newptr == NULL)
		return (error);
	if (mode == -1)
		amd_mmio_write32(sc, regDISP_CFG_UNPLUG, 1);
	else if (mode >= 0 && mode <= 3)
		amd_mmio_write32(sc, regDISP_CFG_PLUG_MODE, mode);
	else
		return (EINVAL);
	return (0);
}

/*
 * dev.atrium_gpu_amd_display.N.lease: "<jail name>:<uid>", or "" for none.
 * Root on the host only: the sysctl is not CTLFLAG_PRISON, so a jailed
 * process cannot write it whatever its uid. Changing the lease does not evict
 * the current holder; it decides the next open.
 */
static int
display_lease_sysctl(SYSCTL_HANDLER_ARGS)
{
	struct atrium_amd_softc *sc = arg1;
	char buf[MAXHOSTNAMELEN + 16];
	char *colon, *end;
	u_long uid;
	int err;

	mtx_lock(&sc->lock);
	if (sc->display_lease_set)
		snprintf(buf, sizeof(buf), "%s:%u", sc->display_lease_jail,
		    (unsigned)sc->display_lease_uid);
	else
		buf[0] = '\0';
	mtx_unlock(&sc->lock);

	err = sysctl_handle_string(oidp, buf, sizeof(buf), req);
	if (err != 0 || req->newptr == NULL)
		return (err);

	if (buf[0] == '\0') {
		mtx_lock(&sc->lock);
		sc->display_lease_set = 0;
		sc->display_lease_jail[0] = '\0';
		mtx_unlock(&sc->lock);
		printf("atrium-display: lease cleared (host only)\n");
		return (0);
	}
	colon = strrchr(buf, ':');
	if (colon == NULL || colon == buf ||
	    (size_t)(colon - buf) >= sizeof(sc->display_lease_jail))
		return (EINVAL);
	*colon = '\0';
	uid = strtoul(colon + 1, &end, 10);
	if (end == colon + 1 || *end != '\0' || uid > UID_MAX)
		return (EINVAL);

	mtx_lock(&sc->lock);
	strlcpy(sc->display_lease_jail, buf, sizeof(sc->display_lease_jail));
	sc->display_lease_uid = (uid_t)uid;
	sc->display_lease_set = 1;
	mtx_unlock(&sc->lock);
	printf("atrium-display: leased to jail %s uid %lu\n", buf, uid);
	return (0);
}

static int
display_open(struct cdev *cdev, int oflags, int devtype, struct thread *td)
{
	struct atrium_amd_softc *sc = cdev->si_drv1;
	int err;

	err = display_lease_check(sc, td->td_ucred);
	if (err != 0)
		return (err);

	/* Exclusive: one holder (the broker) drives the display at a time. */
	mtx_lock(&sc->lock);
	if (sc->display_opened)
		err = EBUSY;
	else
		sc->display_opened = 1;
	mtx_unlock(&sc->lock);
	return (err);
}

static int
display_close(struct cdev *cdev, int fflag, int devtype, struct thread *td)
{
	struct atrium_amd_softc *sc = cdev->si_drv1;

	/*
	 * Hold the last frame: the on-screen buffer (and a flip already handed to
	 * the hardware) stay put until the next holder commits. Only a commit
	 * still waiting on its in-fence is dropped; its holder is gone.
	 */
	mtx_lock(&sc->lock);
	if (sc->display_next_state == 1)
		display_abandon(sc);
	sc->display_opened = 0;
	mtx_unlock(&sc->lock);
	display_drain(sc, td);
	return (0);
}

/* --- kqueue: EVFILT_READ on /dev/atrium-display0 fires once per vblank ----- */

/*
 * The knote list lives in the shared softc (sc->display_sel) and is fired by the
 * GPU module's IH ISR — the block that actually sees the vblank interrupt. We
 * just register/remove knotes on it here; sc->lock (the knlist's lock) guards
 * the list. f_event mirrors EVFILT_TIMER: each vblank the ISR delivers bumps
 * kn_data, and the filter is ready while kn_data != 0. EV_CLEAR (forced below)
 * makes each kevent() return the vblanks elapsed since the last and reset — the
 * natural "wait for the next vblank(s)" edge semantics, retiring WAIT_VBLANK.
 */
static void
display_filt_detach(struct knote *kn)
{
	struct atrium_amd_softc *sc = kn->kn_hook;

	mtx_lock(&sc->lock);
	knlist_remove(&sc->display_sel.si_note, kn, 1);
	mtx_unlock(&sc->lock);
}

static int
display_filt_vblank(struct knote *kn, long hint)
{
	struct atrium_amd_softc *sc = kn->kn_hook;
	uint32_t notes = kn->kn_sfflags &
	    (ATRIUM_DISPLAY_NOTE_RESET | ATRIUM_DISPLAY_NOTE_HOTPLUG);
	uint32_t bit;

	/* Called under sc->lock (the knlist lock) by KNOTE_LOCKED / the kqueue
	 * scan. hint = vblanks this interrupt drained (0 on a bare readiness
	 * scan), DISPLAY_HINT_RESET from the reset restore hook or
	 * DISPLAY_HINT_HOTPLUG from the HPD handler. A registration with notes
	 * in its fflags hears only those, with data = that event's generation;
	 * a plain vblank registration hears everything (data += 1 for an
	 * event). EV_CLEAR zeroes kn_data and kn_fflags after each delivery. */
	if (hint == DISPLAY_HINT_RESET || hint == DISPLAY_HINT_HOTPLUG) {
		bit = hint == DISPLAY_HINT_RESET ? ATRIUM_DISPLAY_NOTE_RESET :
		    ATRIUM_DISPLAY_NOTE_HOTPLUG;
		if (notes == 0) {
			kn->kn_fflags |= bit;
			kn->kn_data += 1;
		} else if (notes & bit) {
			kn->kn_fflags |= bit;
			kn->kn_data = hint == DISPLAY_HINT_RESET ?
			    (int64_t)sc->reset_gen : (int64_t)sc->display_conn_gen;
		}
	} else if (hint != 0 && notes == 0)
		kn->kn_data += hint;
	if (notes != 0)
		return ((kn->kn_fflags & notes) != 0);
	return (kn->kn_data != 0);
}

/*
 * The IH VBLANK cause handler — registered with the base ISR (amd_ih_set_handler
 * in attach). The base ISR drains the device-global IH ring, and for every
 * vblank cookie it routes here, with sc->lock HELD and `count` = how many vblanks
 * this interrupt drained. That is exactly KNOTE_LOCKED's contract (the knlist
 * lock IS sc->lock), so we wake the EVFILT_READ waiters on /dev/atrium-display0
 * directly. The GPU module plays no part — vblank is the display's own signal.
 */
static void
amd_display_vblank_handler(struct atrium_amd_softc *sc, int count)
{
	/* Acknowledge (VERTICAL_INTERRUPT0_CLEAR), keeping it enabled. */
	amd_mmio_write32(sc, regOTG0_OTG_VERTICAL_INTERRUPT0_CONTROL,
	    DCN_VINT0_CLEAR | DCN_VINT0_INT_ENABLE);
	KNOTE_LOCKED(&sc->display_sel.si_note, count);
	display_try_fence(sc);	/* a commit waiting on its render */
	display_reap(sc);
}

/*
 * The IH HPD cause handler (sc->lock held): a monitor came or went. Re-arm
 * for the next change, start a new connector generation (the next CONNECTOR
 * or MODESET re-reads the EDID) and wake NOTE_HOTPLUG listeners. The EDID is
 * not read here: that is up to four 128-byte DDC reads, too slow for an
 * interrupt.
 */
static void
amd_display_hpd_handler(struct atrium_amd_softc *sc, int count __unused)
{
	amd_display_hw_ops.hpd_rearm(sc);
	if (++sc->display_conn_gen == 0)
		sc->display_conn_gen = 1;
	KNOTE_LOCKED(&sc->display_sel.si_note, DISPLAY_HINT_HOTPLUG);
}

/* --- device-reset coordination: the display's prepare/restore hooks --------- */

/*
 * Prepare: a device-wide FLR is about to reset the display block (DCN), so
 * disarm the vblank IRQ — no stale interrupt should fire mid-reset. Called by
 * the base reset coordinator (unlocked).
 */
static void
amd_display_reset_prepare(struct atrium_amd_softc *sc)
{
	mtx_lock(&sc->lock);
	(void)display_frames(sc);	/* bank them: the FLR restarts the OTG */
	mtx_unlock(&sc->lock);
	display_irqs(sc, 0, 0);
}

/*
 * Restore: the FLR is done. Re-apply the last committed state (GPU ABI v2.1
 * §10.2): the mode and the on-screen buffer, then the interrupts. A flip that
 * was in flight when the FLR hit was lost with the hardware queue; count it as
 * landed, since its buffer is the newest one the compositor gave us. The
 * buffer's contents were in VRAM the FLR may have wiped; the compositor's next
 * commit repaints them; NOTE_RESET and STATUS_CONTENT_LOST tell it to.
 */
static void
amd_display_reset_restore(struct atrium_amd_softc *sc)
{
	mtx_lock(&sc->lock);
	if (sc->display_next_state == 2)
		display_landed(sc);
	display_frames_rebase(sc);
	/*
	 * VRAM may not have survived the FLR (§10.3): whatever we re-apply below
	 * may be garbage until the holder commits a fresh frame. Say so in STATUS
	 * and wake the holder's NOTE_RESET (and vblank) knotes.
	 */
	sc->display_content_lost = 1;
	KNOTE_LOCKED(&sc->display_sel.si_note, DISPLAY_HINT_RESET);
	amd_display_hw_ops.hpd_rearm(sc);
	if (sc->display_mode_set && sc->display_on_screen.bo_fp != NULL) {
		if (sc->display_state->timing_valid)
			amd_display_hw_ops.program_timing(sc,
			    &sc->display_state->timing);
		display_program_surface(sc, &sc->display_on_screen, 1);
		display_set_mode(sc);
		display_unlock(sc, 1);
	}
	display_irqs(sc, sc->display_vblank_armed,
	    sc->display_mode_set && sc->display_on_screen.bo_fp != NULL);
	mtx_unlock(&sc->lock);
}

/* --- energy federation: the display is its own power member ----------------- */

/*
 * The display draws real power independent of the GPU — backlight + scanout, with
 * Panel Self-Refresh the dominant lever — so it federates as its own member
 * ("display0") alongside the gpu ("gpu0"). demand reads the model's modeled
 * display power; the budget actuator writes the granted cap, which the device
 * obeys by engaging PSR/VRR when tight (modeled in gpusim's display.rs).
 */
static uint64_t
amd_display_energy_demand_mw(void *arg, int posture __unused)
{
	struct atrium_amd_softc *sc = arg;

	return (amd_mmio_read32(sc, regDISP_POWER_DEMAND_MW));
}

static void
amd_display_energy_actuate(void *arg, uint64_t mw, int posture)
{
	struct atrium_amd_softc *sc = arg;

	/*
	 * Posture set-point (the display maps it to the PSR idle-frame
	 * threshold), then the granted power budget — which still forces PSR
	 * when tight regardless of posture (cap hard, posture soft).
	 */
	amd_mmio_write32(sc, regDISP_POSTURE,
	    posture < 0 ? 0 : (posture > 10 ? 10 : (uint32_t)posture));
	amd_mmio_write32(sc, regDISP_POWER_BUDGET_MW,
	    mw > UINT32_MAX ? UINT32_MAX : (uint32_t)mw);
}

static const struct filterops display_vblank_filtops = {
	.f_isfd = 1,
	.f_detach = display_filt_detach,
	.f_event = display_filt_vblank,
};

static int
display_kqfilter(struct cdev *cdev, struct knote *kn)
{
	struct atrium_amd_softc *sc = display_softc(cdev);

	if (kn->kn_filter != EVFILT_READ ||
	    (kn->kn_sfflags & ~(ATRIUM_DISPLAY_NOTE_RESET |
	    ATRIUM_DISPLAY_NOTE_HOTPLUG)) != 0)
		return (EINVAL);
	kn->kn_fop = &display_vblank_filtops;
	kn->kn_hook = sc;
	kn->kn_flags |= EV_CLEAR;	/* edge: report vblanks-since-last, then reset */
	mtx_lock(&sc->lock);
	knlist_add(&sc->display_sel.si_note, kn, 1);
	mtx_unlock(&sc->lock);
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

	case ATRIUM_DISPLAY_IOC_CONNECTOR:
		return (display_connector(sc,
		    (struct atrium_display_connector_v2 *)data));

	case ATRIUM_DISPLAY_IOC_COMMIT:
		return (display_commit(sc, (struct atrium_display_commit *)data,
		    td));

	case ATRIUM_DISPLAY_IOC_STATUS_V2: {
		struct atrium_display_status_v2 *st =
		    (struct atrium_display_status_v2 *)data;

		if (st->struct_size < sizeof(*st))
			return (EINVAL);
		mtx_lock(&sc->lock);
		display_reap(sc);	/* also the poll-mode (no MSI-X) path */
		st->out_flags = (sc->display_next_state != 0 ?
		    ATRIUM_DISPLAY_STATUS_COMMIT_PENDING : 0) |
		    (sc->display_mode_set ? ATRIUM_DISPLAY_STATUS_MODE_SET : 0) |
		    (sc->display_content_lost ?
		    ATRIUM_DISPLAY_STATUS_CONTENT_LOST : 0);
		st->out_vblank_count = display_frames(sc);
		st->out_flips_done = sc->display_flips_landed;
		st->out_dropped_flips = amd_mmio_read32(sc, regDISP_DROPPED_FLIPS);
		st->out_tear_line = amd_mmio_read32(sc, regDISP_TEAR_LINE);
		st->out_refresh_mhz = sc->display_refresh_mhz;
		st->out_last_fault = sc->display_last_fault;
		mtx_unlock(&sc->lock);
		display_drain(sc, td);
		return (0);
	}

	case ATRIUM_DISPLAY_IOC_STATUS: {
		struct atrium_display_status *st =
		    (struct atrium_display_status *)data;

		mtx_lock(&sc->lock);
		st->vblank_count = display_frames(sc);
		mtx_unlock(&sc->lock);
		st->dropped_flips = amd_mmio_read32(sc, regDISP_DROPPED_FLIPS);
		st->tear_line = amd_mmio_read32(sc, regDISP_TEAR_LINE);
		return (0);
	}

	case ATRIUM_DISPLAY_IOC_CONFIG: {
		struct atrium_display_config *c =
		    (struct atrium_display_config *)data;

		amd_mmio_write32(sc, regDISP_CFG_CONNECTOR_TYPE, c->connector_type);
		if (c->plug_mode == ATRIUM_DISPLAY_UNPLUG)
			amd_mmio_write32(sc, regDISP_CFG_UNPLUG, 1);
		else
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
	.d_close =	display_close,
	.d_ioctl =	display_ioctl,
	.d_kqfilter =	display_kqfilter,
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
	sc->display_state = malloc(sizeof(*sc->display_state), M_ATRIUM_DISPLAY,
	    M_WAITOK | M_ZERO);
	sc->display_conn_gen = 1;
	sc->display_cdev->si_drv1 = sc;
	TASK_INIT(&sc->display_fence_task, 0, display_fence_task, sc);

	/* Who may open it (display_lease_check); jaild sets it for frescod. */
	SYSCTL_ADD_PROC(device_get_sysctl_ctx(dev),
	    SYSCTL_CHILDREN(device_get_sysctl_tree(dev)), OID_AUTO, "lease",
	    CTLTYPE_STRING | CTLFLAG_RW | CTLFLAG_MPSAFE, sc, 0,
	    display_lease_sysctl, "A",
	    "jail:uid that may open the display; empty = host only");
	SYSCTL_ADD_PROC(device_get_sysctl_ctx(dev),
	    SYSCTL_CHILDREN(device_get_sysctl_tree(dev)), OID_AUTO, "sim_plug",
	    CTLTYPE_INT | CTLFLAG_WR | CTLFLAG_MPSAFE, sc, 0,
	    display_sim_plug_sysctl, "I",
	    "simulator: plug monitor 0 VGA, 1 1080p, 2 4K, 3 desktop; -1 unplug");

	/*
	 * Register our IH cause handler so the base ISR routes DCN vblank interrupts
	 * here (→ the EVFILT_READ knote). This is all the display needs from the
	 * interrupt path — no MSI-X or ISR of its own — and it works whether or not
	 * the gpu (render) module is loaded.
	 */
	amd_ih_set_handler(sc, ATRIUM_AMD_IH_CAUSE_VBLANK,
	    amd_display_vblank_handler);
	amd_ih_set_handler(sc, ATRIUM_AMD_IH_CAUSE_FLIP,
	    amd_display_flip_handler);
	amd_ih_set_handler(sc, ATRIUM_AMD_IH_CAUSE_HPD,
	    amd_display_hpd_handler);
	mtx_lock(&sc->lock);
	amd_display_hw_ops.hpd_rearm(sc);	/* hotplug interrupts from now on */
	mtx_unlock(&sc->lock);

	/*
	 * Register the display's reset hooks so a coordinated device reset (a
	 * userspace GPU reset routed through the base) quiesces + re-arms the display
	 * block instead of leaving it dead.
	 */
	amd_reset_register(sc, ATRIUM_AMD_IP_DISPLAY, amd_display_reset_prepare,
	    amd_display_reset_restore);

	/*
	 * Federate the display's energy: it draws power independently of the gpu
	 * (backlight + scanout), so it joins the Laminar energy allocator as its own
	 * member, "display0", alongside the gpu's "gpu0".
	 */
	sc->display_energy_member = energy_member_register("display0",
	    amd_display_energy_demand_mw, amd_display_energy_actuate, sc, 1);

	device_printf(dev, "ready: /dev/atrium-display%d (energy member %d)\n",
	    device_get_unit(dev), sc->display_energy_member);
	return (0);
}

static int
atrium_display_detach(device_t dev)
{
	struct atrium_amd_softc *sc = device_get_softc(device_get_parent(dev));

	/*
	 * Stop vblank interrupts at the device, then clear our handler under
	 * sc->lock (serializes against a firing ISR) so the base never routes into
	 * this module's text after it unloads.
	 */
	if (sc->display_energy_member >= 0) {
		energy_member_unregister(sc->display_energy_member);
		sc->display_energy_member = -1;
	}
	display_irqs(sc, 0, 0);
	sc->display_vblank_armed = 0;
	amd_ih_set_handler(sc, ATRIUM_AMD_IH_CAUSE_VBLANK, NULL);
	amd_ih_set_handler(sc, ATRIUM_AMD_IH_CAUSE_FLIP, NULL);
	amd_mmio_write32(sc, regHPD0_DC_HPD_INT_CONTROL, DCN_HPD_INT_ACK);
	amd_ih_set_handler(sc, ATRIUM_AMD_IH_CAUSE_HPD, NULL);
	amd_reset_register(sc, ATRIUM_AMD_IP_DISPLAY, NULL, NULL);
	if (sc->display_cdev != NULL) {
		destroy_dev(sc->display_cdev);
		sc->display_cdev = NULL;
	}
	/*
	 * Let go of every buffer and fence the display still holds. The screen
	 * keeps whatever the hardware last latched; nothing can flip it now.
	 */
	mtx_lock(&sc->lock);
	if (sc->display_next_state != 0)
		display_abandon(sc);	/* clears the in-fence watch */
	display_park(sc, sc->display_on_screen.bo_fp);
	memset(&sc->display_on_screen, 0, sizeof(sc->display_on_screen));
	sc->display_mode_set = 0;
	sc->display_mode_width = 0;
	sc->display_mode_height = 0;
	sc->display_opened = 0;
	mtx_unlock(&sc->lock);
	taskqueue_drain(taskqueue_thread, &sc->display_fence_task);
	display_drain(sc, curthread);
	free(sc->display_state, M_ATRIUM_DISPLAY);
	sc->display_state = NULL;
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
