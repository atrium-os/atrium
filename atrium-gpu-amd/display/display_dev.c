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
 * Scanout is by object (display ABI, include/atrium/display_abi_v2.h): a
 * commit names the BO by its fd. This module resolves the fd through the gpu
 * module's exported vtable (sc->backend; it links against nothing there),
 * holds a file reference on the BO while it may be on screen, waits on the
 * commit's in-fence before flipping and signals its out-fence when the flip
 * lands (IH cause FLIP). Userspace never supplies an address. The node is
 * exclusive, and what is on screen stays there when its holder closes.
 *
 * Several outputs (display ABI 3.0): DCN 4's four pipes each drive their own
 * connector, and each is an output here with its own state (struct
 * display_output): its monitor, its mode, what it scans out, its pending
 * flip and fences, its vblank counter. Flips are per output, so monitors at
 * different rates never wait on each other. Topology changes (light, re-mode,
 * turn off) go through one atomic COMMIT_MULTI: checked as a whole, the
 * shared bandwidth included, then applied, and rolled back if the hardware
 * still refuses an output. The 2.x ioctls act on output 0.
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

#define DISPLAY_NOUT		DCN_PIPES
#define DISPLAY_NRELEASE	(DISPLAY_NOUT * 8)

CTASSERT(DISPLAY_NOUT <= ATRIUM_DISPLAY_MAX_OUTPUTS);

/* A buffer the display may be scanning out, and the reference it holds. */
struct display_fb {
	struct file	*bo_fp;		/* held ref, NULL = none */
	uint64_t	 vram_offset;
	uint64_t	 size;
	uint32_t	 pitch;
	uint32_t	 format;
	uint64_t	 reset_gen;	/* sc->reset_gen when committed */
};

/*
 * One output: a DCN pipe and the connector it drives. Guarded by sc->lock.
 *
 * Commit state. The display holds a file reference on every BO it may be
 * scanning out, so a client closing its bo_fd can't free a buffer that is
 * still on screen:
 *   on_screen  the BO the pipe reads now (kept across close: hold-last-frame)
 *   next       a committed BO that hasn't landed yet (waiting on its
 *              in-fence, or on vblank), with its fences
 * References are only dropped in process context, so the interrupt path
 * parks the ones it releases (display_park) for the next ioctl, close or
 * detach to fdrop.
 *
 * Connector state:
 *   conn_gen     topo_gen at this output's last hotplug (never 0)
 *   sink         the monitor as last probed (raw EDID and its modes)
 *   sink_gen     conn_gen when it was probed (0 = never)
 *   sink_epoch   bumped by display_sink_invalidate: a probe that began
 *                before it is not cached
 *   identity     the monitor's (atrium_edid_identity), with sink
 *   timing       the timing of the lit mode, re-applied after a device reset
 *   vrr_min_hz   its variable refresh floor (0 = fixed refresh), ditto
 *   hdr          (2.3) the EOTF, colorimetry and metadata being signalled,
 *                re-sent after a device reset
 *   hdr_pending  what HDR SET asked for, applied by the next commit
 *
 * Span state (3.1, COMMIT_SYNC_GROUP):
 *   sync_group   the timing group it belongs to (0 = none)
 *   flip_lock    its flips land with the group's other locked members
 *   sync_vrr_hz  the group's variable refresh floor, the highest of its
 *                members' (their ranges' intersection), which every member
 *                runs while in the group; 0 = its own
 */
struct display_output {
	struct atrium_amd_softc	*sc;
	uint32_t		 n;		/* pipe and connector index */

	struct display_fb	 on_screen, next;
	int			 content_lost;	/* STATUS_CONTENT_LOST */
	int			 next_state;	/* 0 none, 1 waiting on in-fence, 2 flipped */
	int			 next_vsync;
	uint64_t		 flips_landed;	/* commits that reached the screen */
	uint32_t		 width;		/* the mode's width, for a packed pitch */
	uint32_t		 frame_raw;	/* last OTG_STATUS_FRAME_COUNT read */
	uint64_t		 frames;	/* frames, widened past the 24-bit counter */
	struct file		*in_fp;		/* in-fence syncobj (held ref) */
	struct atrium_amd_syncobj *in_so;
	uint64_t		 in_value;
	struct file		*out_fp;	/* out-fence syncobj (held ref) */
	struct atrium_amd_syncobj *out_so;
	uint64_t		 out_value;
	int			 mode_set;	/* CRTC lit by a MODESET commit */
	uint32_t		 refresh_mhz;	/* refresh of the mode in use */
	uint32_t		 mode_width;	/* the mode in use, for the kernel's */
	uint32_t		 mode_height;	/* own scanout bounds check */
	uint32_t		 last_fault;	/* fault of the last deferred flip */
	struct task		 fence_task;	/* in-fence signalled: try to flip */

	uint32_t		 conn_gen;
	struct atrium_display_sink sink;
	uint32_t		 sink_gen;
	uint32_t		 sink_epoch;
	uint64_t		 identity;
	struct atrium_display_timing timing;
	int			 timing_valid;
	uint32_t		 vrr_min_hz;
	struct atrium_display_hdr hdr;
	struct atrium_display_hdr hdr_pending;
	int			 hdr_pending_valid;
	uint32_t		 sync_group;
	int			 flip_lock;
	uint32_t		 sync_vrr_hz;
};

/* The display module's state, hung off the shared softc as sc->display_state. */
struct atrium_amd_display_state {
	struct display_output	 out[DISPLAY_NOUT];
	uint32_t		 topo_gen;	/* bumped by a hotplug on any output */
	struct file		*release[DISPLAY_NRELEASE];
	int			 nrelease;
};

static MALLOC_DEFINE(M_ATRIUM_DISPLAY, "atrium_display", "Atrium display state");

/*
 * KNOTE hints. A vblank is (output << 24) | count (count > 0, so a vblank
 * hint is never 0, the bare readiness scan); the events are negative.
 */
#define DISPLAY_HINT_RESET	LONG_MIN
#define DISPLAY_HINT_HOTPLUG	(LONG_MIN + 1)
#define DISPLAY_HINT_VBLANK(n, count) \
	(((long)(n) << 24) | (long)((count) > 0xffffff ? 0xffffff : (count)))

static struct atrium_amd_softc *
display_softc(struct cdev *cdev)
{
	return (cdev->si_drv1);
}

static struct display_output *
display_out(struct atrium_amd_softc *sc, uint32_t n)
{
	return (n < DISPLAY_NOUT ? &sc->display_state->out[n] : NULL);
}

/* An output's instance of a pipe-0 register. */
#define HUBP(o, r)	DCN_PIPE_REG(r, DCN_STRIDE_HUBP, (o)->n)
#define OTG(o, r)	DCN_PIPE_REG(r, DCN_STRIDE_OTG, (o)->n)
#define HPD(o, r)	DCN_PIPE_REG(r, DCN_STRIDE_HPD, (o)->n)
#define DIG(o, r)	DCN_PIPE_REG(r, DCN_STRIDE_DIG, (o)->n)
#define SIMR(o, r)	regDISP_PIPE(r, (o)->n)

static inline uint32_t
o_rd(struct display_output *o, uint32_t reg)
{
	return (amd_mmio_read32(o->sc, reg));
}

static inline void
o_wr(struct display_output *o, uint32_t reg, uint32_t v)
{
	amd_mmio_write32(o->sc, reg, v);
}

/*
 * --- the AMD (DCN 4) side of atrium_display_hw_ops ---------------------------
 *
 * Everything below the generic connector code (atrium-display/display_hw.h):
 * how this hardware senses a monitor (HPDn), reads its EDID (DDC; the SIM
 * byte port until D5 brings up the real I2C engine) and programs a timing
 * (OTGn's timing registers, the pixel clock on a SIM register until DCCG is
 * modelled). `hw` is the output.
 */
static int
amd_disp_sense(void *hw)
{
	struct display_output *o = hw;

	return ((o_rd(o, HPD(o, regHPD0_DC_HPD_INT_STATUS)) &
	    DCN_HPD_SENSE) != 0);
}

static int
amd_disp_read_edid_block(void *hw, uint32_t n, uint8_t *out)
{
	struct display_output *o = hw;
	uint32_t i;

	for (i = 0; i < ATRIUM_EDID_BLOCK; i++) {
		o_wr(o, SIMR(o, regDISP_DDC_OFFSET), n * ATRIUM_EDID_BLOCK + i);
		out[i] = (uint8_t)o_rd(o, SIMR(o, regDISP_DDC_DATA));
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
	struct display_output *o = hw;
	uint32_t hb = t->hsync + t->hback, vb = t->vsync + t->vback;

	o_wr(o, OTG(o, regOTG0_OTG_H_TOTAL),
	    t->hactive + t->hfront + t->hsync + t->hback - 1);
	o_wr(o, OTG(o, regOTG0_OTG_H_BLANK_START_END),
	    (hb + t->hactive) | (hb << DCN_OTG_END_SHIFT));
	o_wr(o, OTG(o, regOTG0_OTG_H_SYNC_A), t->hsync << DCN_OTG_END_SHIFT);
	o_wr(o, OTG(o, regOTG0_OTG_V_TOTAL),
	    t->vactive + t->vfront + t->vsync + t->vback - 1);
	o_wr(o, OTG(o, regOTG0_OTG_V_BLANK_START_END),
	    (vb + t->vactive) | (vb << DCN_OTG_END_SHIFT));
	o_wr(o, OTG(o, regOTG0_OTG_V_SYNC_A), t->vsync << DCN_OTG_END_SHIFT);
	o_wr(o, SIMR(o, regDISP_PIXEL_CLOCK_KHZ), t->pixel_clock_khz);
}

/*
 * Variable refresh on OTGn (DRR): V_TOTAL_MIN is the mode's own total,
 * V_TOTAL_MAX the total that stretches it to `min_hz`, and selecting both in
 * V_TOTAL_CONTROL lets each frame end anywhere between, at the flip that
 * waits on it. Field use as the Linux display code (gpl_fact, medium
 * confidence). min_hz 0: both at the mode's total, selects off.
 */
static void
amd_disp_set_vrr(void *hw, const struct atrium_display_timing *t,
    uint32_t min_hz)
{
	struct display_output *o = hw;
	uint32_t vt = t->vactive + t->vfront + t->vsync + t->vback;
	uint32_t vmax = min_hz != 0 ? atrium_timing_vtotal_at(t, min_hz) : vt;

	if (vmax == 0 || vmax > DCN_OTG_TOTAL_MASK + 1)
		vmax = DCN_OTG_TOTAL_MASK + 1;
	o_wr(o, OTG(o, regOTG0_OTG_V_TOTAL_CONTROL), 0);
	o_wr(o, OTG(o, regOTG0_OTG_V_TOTAL_MIN), vt - 1);
	o_wr(o, OTG(o, regOTG0_OTG_V_TOTAL_MAX), vmax - 1);
	if (min_hz != 0)
		o_wr(o, OTG(o, regOTG0_OTG_V_TOTAL_CONTROL),
		    DCN_V_TOTAL_MIN_SEL | DCN_V_TOTAL_MAX_SEL);
}

/*
 * HPDn is a level interrupt that fires while SENSE equals POLARITY: wait for
 * the opposite of what is there now (a disconnect while a monitor is
 * present, a connect while none is), then acknowledge.
 */
static void
amd_disp_hpd_rearm(void *hw)
{
	struct display_output *o = hw;
	int present;

	o_wr(o, HPD(o, regHPD0_DC_HPD_CONTROL), DCN_HPD_EN);
	present = amd_disp_sense(o);
	o_wr(o, HPD(o, regHPD0_DC_HPD_INT_CONTROL), DCN_HPD_INT_EN |
	    (present ? 0 : DCN_HPD_INT_POLARITY) | DCN_HPD_INT_ACK);
}

/*
 * Infoframes through VPGn's generic packets, sent every frame by DIGn:
 * generic k holds frames[k]. Packets past `n` stop being sent. Packet RAM
 * layout and the FRAME_UPDATE hand-over as the Linux display code
 * (gpl_fact, medium confidence).
 */
static void
amd_disp_send_infoframes(void *hw, const uint8_t *frames, uint32_t n)
{
	struct display_output *o = hw;
	uint32_t ctl = 0, k, d;
	const uint8_t *f;

	if (n > DCN_VPG_GENERICS)
		n = DCN_VPG_GENERICS;
	for (k = 0; k < n; k++) {
		f = frames + k * ATRIUM_INFOFRAME_LEN;
		o_wr(o, DIG(o, regVPG0_VPG_GENERIC_PACKET_ACCESS_CTRL),
		    k * DCN_VPG_PACKET_DWORDS);
		for (d = 0; d < DCN_VPG_PACKET_DWORDS; d++)
			o_wr(o, DIG(o, regVPG0_VPG_GENERIC_PACKET_DATA),
			    (uint32_t)f[4 * d] | (uint32_t)f[4 * d + 1] << 8 |
			    (uint32_t)f[4 * d + 2] << 16 |
			    (uint32_t)f[4 * d + 3] << 24);
		ctl |= DCN_HDMI_GENERIC_SEND_CONT(k);
	}
	if (n != 0)
		o_wr(o, DIG(o, regVPG0_VPG_GSP_FRAME_UPDATE_CTRL),
		    (1u << n) - 1);
	o_wr(o, DIG(o, regDIG0_HDMI_GENERIC_PACKET_CONTROL0), ctl);
}

static const struct atrium_display_hw_ops amd_display_hw_ops = {
	.sense =		amd_disp_sense,
	.read_edid_block =	amd_disp_read_edid_block,
	.program_timing =	amd_disp_program_timing,
	.hpd_rearm =		amd_disp_hpd_rearm,
	.set_vrr =		amd_disp_set_vrr,
	.send_infoframes =	amd_disp_send_infoframes,
};


/*
 * The monitor on `o` as of its current hotplug generation, into `out`;
 * returns that generation (and the identity in *identity if asked). Probing
 * reads up to four EDID blocks, so it runs unlocked, and only when a hotplug
 * has happened since the last probe.
 */
static uint32_t
display_sink_get(struct display_output *o, struct atrium_display_sink *out,
    uint64_t *identity)
{
	struct atrium_amd_softc *sc = o->sc;
	uint32_t gen, epoch;
	uint64_t id;

	mtx_lock(&sc->lock);
	gen = o->conn_gen;
	if (o->sink_gen == gen) {
		*out = o->sink;
		if (identity != NULL)
			*identity = o->identity;
		mtx_unlock(&sc->lock);
		return (gen);
	}
	epoch = o->sink_epoch;
	mtx_unlock(&sc->lock);
	(void)atrium_display_probe_sink(&amd_display_hw_ops, o, out);
	id = out->connected ? atrium_edid_identity(out->edid, out->edid_len,
	    o->n) : 0;
	if (identity != NULL)
		*identity = id;
	mtx_lock(&sc->lock);
	if (o->conn_gen == gen && o->sink_epoch == epoch) {
		o->sink = *out;
		o->identity = id;
		o->sink_gen = gen;
	}
	mtx_unlock(&sc->lock);
	return (gen);
}

/*
 * The kernel itself just changed the monitor (the simulator's CONFIG, USB-C
 * and sim_plug knobs): probe it afresh on the next use rather than wait for
 * the hotplug interrupt to say so, which may land after the caller's next
 * ioctl. The generation is left to the interrupt.
 */
static void
display_sink_invalidate(struct display_output *o)
{
	mtx_lock(&o->sc->lock);
	o->sink_gen = 0;
	o->sink_epoch++;
	mtx_unlock(&o->sc->lock);
}

/*
 * Interim ENUM: HPD, the §8 type and the EDID base block. A disconnected
 * connector reads all 0xff, so the caller's parse fails its header (the
 * realistic "unplugged" failure mode).
 */
static void
display_enum(struct display_output *o, struct atrium_display_connector *c)
{
	struct atrium_display_sink *sink;

	sink = malloc(sizeof(*sink), M_ATRIUM_DISPLAY, M_WAITOK);
	(void)display_sink_get(o, sink, NULL);
	c->connected = sink->connected;
	c->connector_type = o_rd(o, SIMR(o, regDISP_CONNECTOR_TYPE));
	c->usbc_lanes = o_rd(o, SIMR(o, regDISP_USBC_LANES));
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
display_modes(struct display_output *o, struct atrium_display_modes *out)
{
	struct atrium_display_sink *sink;
	struct atrium_display_mode_v2 m;
	uint32_t i;

	sink = malloc(sizeof(*sink), M_ATRIUM_DISPLAY, M_WAITOK);
	(void)display_sink_get(o, sink, NULL);
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

/* The CONNECTOR_* flags for `o` with `sink` attached. Lock held. */
static uint32_t
display_conn_flags(struct display_output *o,
    const struct atrium_display_sink *sink, int current_listed)
{
	uint32_t f = 0;

	mtx_assert(&o->sc->lock, MA_OWNED);
	if (sink->connected)
		f |= ATRIUM_DISPLAY_CONNECTOR_CONNECTED;
	if (sink->vrr_min_hz != 0 && amd_display_hw_ops.set_vrr != NULL)
		f |= ATRIUM_DISPLAY_CONNECTOR_VRR;
	if (current_listed && o->vrr_min_hz != 0)
		f |= ATRIUM_DISPLAY_CONNECTOR_VRR_ACTIVE;
	if (sink->connected && sink->hdr &&
	    amd_display_hw_ops.send_infoframes != NULL)
		f |= ATRIUM_DISPLAY_CONNECTOR_HDR;
	if (o->mode_set && o->hdr.eotf != ATRIUM_DISPLAY_EOTF_SDR)
		f |= ATRIUM_DISPLAY_CONNECTOR_HDR_ACTIVE;
	return (f);
}

/* CONNECTOR (2.1): the monitor, its raw EDID and every mode it offers. */
static int
display_connector(struct display_output *o,
    struct atrium_display_connector_v2 *c, uint64_t *identity)
{
	struct atrium_display_sink *sink;
	uint32_t i, n;

	if (c->struct_size < sizeof(*c))
		return (EINVAL);
	sink = malloc(sizeof(*sink), M_ATRIUM_DISPLAY, M_WAITOK);
	c->out_generation = display_sink_get(o, sink, identity);
	c->out_connector_type = o_rd(o, SIMR(o, regDISP_CONNECTOR_TYPE));
	n = sink->edid_len < ATRIUM_DISPLAY_EDID_MAX ? sink->edid_len :
	    ATRIUM_DISPLAY_EDID_MAX;
	memset(c->out_edid, 0, sizeof(c->out_edid));
	memcpy(c->out_edid, sink->edid, n);
	c->out_edid_len = n;
	c->out_mode_count = 0;
	c->out_current_mode = 0xffffffffu;
	c->out_vrr_min_hz = (uint16_t)sink->vrr_min_hz;
	c->out_vrr_max_hz = (uint16_t)sink->vrr_max_hz;
	if (sink->vrr_min_hz == 0 || amd_display_hw_ops.set_vrr == NULL)
		c->out_vrr_min_hz = c->out_vrr_max_hz = 0;
	memset(c->out_modes, 0, sizeof(c->out_modes));
	mtx_lock(&o->sc->lock);
	for (i = 0; i < sink->ntimings && i < ATRIUM_DISPLAY_CONNECTOR_MODES;
	    i++) {
		display_mode_v2(&sink->timings[i], &c->out_modes[i]);
		if (o->mode_set && o->timing_valid &&
		    atrium_timing_equal(&o->timing, &sink->timings[i]))
			c->out_current_mode = i;
		c->out_mode_count++;
	}
	c->out_flags = display_conn_flags(o, sink,
	    c->out_current_mode != 0xffffffffu);
	mtx_unlock(&o->sc->lock);
	free(sink, M_ATRIUM_DISPLAY);
	return (0);
}

/* --- HDR (display ABI 2.3) ------------------------------------------------- */

/* The EOTFs and colorimetries this monitor can be sent, as HDR's out_*
 * bit masks: SDR and the default always; the rest from its EDID, and only
 * if the encoder can send infoframes. */
static void
display_hdr_offers(const struct atrium_display_sink *sink, uint32_t *eotfs,
    uint32_t *colorimetry)
{
	*eotfs = 1u << ATRIUM_DISPLAY_EOTF_SDR;
	*colorimetry = 1u << ATRIUM_DISPLAY_COLORIMETRY_DEFAULT;
	if (!sink->connected || !sink->hdr ||
	    amd_display_hw_ops.send_infoframes == NULL)
		return;
	*eotfs |= sink->hdr_caps.eotfs & 0xfu;
	if (sink->hdr_caps.bt2020_rgb)
		*colorimetry |= 1u << ATRIUM_DISPLAY_COLORIMETRY_BT2020;
}

/* Does the monitor take `h`'s EOTF and colorimetry? */
static int
display_hdr_fits(const struct atrium_display_sink *sink,
    const struct atrium_display_hdr *h)
{
	uint32_t eotfs, colorimetry;

	display_hdr_offers(sink, &eotfs, &colorimetry);
	return (h->eotf < 32 && (eotfs & (1u << h->eotf)) != 0 &&
	    h->colorimetry < 32 && (colorimetry & (1u << h->colorimetry)) != 0);
}

/* Signal `h`: a DRM infoframe for a non-SDR EOTF, an AVI infoframe for any
 * HDR or BT.2020 stream, nothing at all for plain SDR. Lock held. */
static void
display_hdr_send(struct display_output *o, const struct atrium_display_hdr *h)
{
	struct atrium_hdr_metadata m;
	uint8_t frames[2 * ATRIUM_INFOFRAME_LEN];
	uint32_t n = 0, i;

	mtx_assert(&o->sc->lock, MA_OWNED);
	if (amd_display_hw_ops.send_infoframes == NULL)
		return;
	if (h->eotf != ATRIUM_DISPLAY_EOTF_SDR) {
		memset(&m, 0, sizeof(m));
		m.eotf = (uint8_t)h->eotf;
		for (i = 0; i < 3; i++) {
			m.primaries_x[i] = h->primaries_x[i];
			m.primaries_y[i] = h->primaries_y[i];
		}
		m.white_x = h->white_x;
		m.white_y = h->white_y;
		m.max_mastering_nits = h->max_mastering_nits;
		m.min_mastering = h->min_mastering;
		m.max_cll = h->max_cll;
		m.max_fall = h->max_fall;
		atrium_infoframe_drm(&m, &frames[n++ * ATRIUM_INFOFRAME_LEN]);
	}
	if (h->eotf != ATRIUM_DISPLAY_EOTF_SDR ||
	    h->colorimetry != ATRIUM_DISPLAY_COLORIMETRY_DEFAULT)
		atrium_infoframe_avi(
		    h->colorimetry == ATRIUM_DISPLAY_COLORIMETRY_BT2020, 0,
		    &frames[n++ * ATRIUM_INFOFRAME_LEN]);
	amd_display_hw_ops.send_infoframes(o, frames, n);
}

/*
 * A commit succeeded on `o`: what HDR SET asked for goes out with it. A
 * MODESET also re-checks the signalled state against `sink` (the monitor it
 * lit): one that no longer takes it gets SDR. Lock held.
 */
static void
display_hdr_commit(struct display_output *o,
    const struct atrium_display_sink *sink)
{
	int changed = 0;

	mtx_assert(&o->sc->lock, MA_OWNED);
	if (o->hdr_pending_valid) {
		o->hdr = o->hdr_pending;
		o->hdr_pending_valid = 0;
		changed = 1;
	}
	if (sink != NULL && !display_hdr_fits(sink, &o->hdr)) {
		memset(&o->hdr, 0, sizeof(o->hdr));
		changed = 1;
	}
	if (changed || sink != NULL)
		display_hdr_send(o, &o->hdr);
}

/* HDR (2.3): report the monitor and the signalled state, or SET the next. */
static int
display_hdr(struct atrium_amd_softc *sc, struct atrium_display_hdr *h)
{
	struct display_output *o;
	struct atrium_display_sink *sink;
	struct atrium_display_hdr want;
	uint32_t output;
	int err = 0;

	if (h->struct_size < sizeof(*h) ||
	    (h->flags & ~ATRIUM_DISPLAY_HDR_SET) != 0 ||
	    (o = display_out(sc, h->output)) == NULL)
		return (EINVAL);
	output = h->output;
	sink = malloc(sizeof(*sink), M_ATRIUM_DISPLAY, M_WAITOK);
	(void)display_sink_get(o, sink, NULL);
	if (h->flags & ATRIUM_DISPLAY_HDR_SET) {
		if (!sink->connected)
			err = ENODEV;
		else if (h->eotf > ATRIUM_DISPLAY_EOTF_HLG ||
		    h->colorimetry > ATRIUM_DISPLAY_COLORIMETRY_BT2020 ||
		    !display_hdr_fits(sink, h))
			err = EINVAL;
		if (err == 0) {
			want = *h;
			want.flags = 0;
			want.out_eotfs = want.out_colorimetry = 0;
			want.out_max_nits = want.out_max_fall_nits = 0;
			want.out_min_nits_x10000 = 0;
			mtx_lock(&sc->lock);
			o->hdr_pending = want;
			o->hdr_pending_valid = 1;
			mtx_unlock(&sc->lock);
		}
	} else {
		mtx_lock(&sc->lock);
		want = o->hdr;
		mtx_unlock(&sc->lock);
		h->eotf = want.eotf;
		h->colorimetry = want.colorimetry;
		memcpy(h->primaries_x, want.primaries_x, sizeof(h->primaries_x));
		memcpy(h->primaries_y, want.primaries_y, sizeof(h->primaries_y));
		h->white_x = want.white_x;
		h->white_y = want.white_y;
		h->max_mastering_nits = want.max_mastering_nits;
		h->min_mastering = want.min_mastering;
		h->max_cll = want.max_cll;
		h->max_fall = want.max_fall;
	}
	display_hdr_offers(sink, &h->out_eotfs, &h->out_colorimetry);
	if ((h->out_eotfs & ~1u) != 0) {
		h->out_max_nits = atrium_hdr_cv_nits(sink->hdr_caps.max_cv);
		h->out_max_fall_nits =
		    atrium_hdr_cv_nits(sink->hdr_caps.max_fall_cv);
		h->out_min_nits_x10000 = atrium_hdr_min_cv_ten_thousandths(
		    sink->hdr_caps.max_cv, sink->hdr_caps.min_cv);
	} else
		h->out_max_nits = h->out_max_fall_nits =
		    h->out_min_nits_x10000 = 0;
	h->output = output;
	free(sink, M_ATRIUM_DISPLAY);
	return (err);
}

/* --- commit state machine (display ABI v2, per output) ------------------- */

/*
 * The display keeps a file reference on each BO it may be scanning out, and on
 * each fence it still has to wait on or signal. The interrupt path runs with
 * sc->lock held and can't fdrop, so it parks references here; display_drain
 * drops them later in process context.
 */
static void
display_park(struct atrium_amd_softc *sc, struct file *fp)
{
	struct atrium_amd_display_state *ds = sc->display_state;

	mtx_assert(&sc->lock, MA_OWNED);
	if (fp == NULL)
		return;
	/* At most one commit is outstanding per output and every ioctl
	 * drains, so a landing parks at most three references per output
	 * before the next drain (a topology commit parks a few more). */
	KASSERT(ds->nrelease < DISPLAY_NRELEASE,
	    ("atrium display: release list full"));
	ds->release[ds->nrelease++] = fp;
}

static void
display_drain(struct atrium_amd_softc *sc, struct thread *td)
{
	struct atrium_amd_display_state *ds = sc->display_state;
	struct file *fps[DISPLAY_NRELEASE];
	int i, n;

	mtx_lock(&sc->lock);
	n = ds->nrelease;
	for (i = 0; i < n; i++) {
		fps[i] = ds->release[i];
		ds->release[i] = NULL;
	}
	ds->nrelease = 0;
	mtx_unlock(&sc->lock);
	for (i = 0; i < n; i++)
		fdrop(fps[i], td);
}

/*
 * Program HUBPn to scan out `fb`, under SURFACE_UPDATE_LOCK: the address write
 * that would otherwise request a flip is held until display_unlock(). The
 * surface address is the BO's VRAM offset (the display reads VRAM by offset;
 * the referee checks residency and bounds and reports through regDISP_FAULT).
 * XRGB8888 linear is DCN's 32 bpp ARGB8888, linear swizzle, alpha plane off;
 * XRGB2101010 is its ARGB2101010.
 */
static void
display_program_surface(struct display_output *o, const struct display_fb *fb,
    int vsync)
{
	uint32_t pitch = fb->pitch != 0 ? fb->pitch : o->width * 4;
	uint32_t type = vsync ? 0 : DCN_SURFACE_FLIP_TYPE;

	o_wr(o, HUBP(o, regHUBPREQ0_DCSURF_FLIP_CONTROL),
	    DCN_SURFACE_UPDATE_LOCK | type);
	o_wr(o, HUBP(o, regHUBP0_DCSURF_SURFACE_CONFIG),
	    fb->format == ATRIUM_DISPLAY_FORMAT_XRGB2101010 ?
	    DCN_PIXEL_FORMAT_ARGB2101010 : DCN_PIXEL_FORMAT_ARGB8888);
	o_wr(o, HUBP(o, regHUBP0_DCSURF_TILING_CONFIG), DCN_SW_MODE_LINEAR);
	o_wr(o, HUBP(o, regHUBPREQ0_DCSURF_SURFACE_PITCH),
	    pitch >= 4 ? (pitch / 4 - 1) & DCN_PITCH_MASK : 0);
	o_wr(o, HUBP(o, regHUBPREQ0_DCSURF_PRIMARY_SURFACE_ADDRESS_HIGH),
	    (uint32_t)(fb->vram_offset >> 32) & DCN_ADDRESS_HIGH_MASK);
	o_wr(o, HUBP(o, regHUBPREQ0_DCSURF_PRIMARY_SURFACE_ADDRESS),
	    (uint32_t)fb->vram_offset);
}

/* Release SURFACE_UPDATE_LOCK: a held surface update becomes a flip, at
 * vsync or at once as display_program_surface chose. */
static void
display_unlock(struct display_output *o, int vsync)
{
	o_wr(o, HUBP(o, regHUBPREQ0_DCSURF_FLIP_CONTROL),
	    vsync ? 0 : DCN_SURFACE_FLIP_TYPE);
}

/* Arm (or disarm) OTGn's vertical interrupt 0, our per-frame interrupt, and
 * HUBPn's flip interrupt; both acknowledge anything latched. */
static void
display_irqs(struct display_output *o, int vblank, int flip)
{
	o_wr(o, OTG(o, regOTG0_OTG_VERTICAL_INTERRUPT0_CONTROL),
	    DCN_VINT0_CLEAR | (vblank ? DCN_VINT0_INT_ENABLE : 0));
	o_wr(o, HUBP(o, regHUBPREQ0_DCSURF_SURFACE_FLIP_INTERRUPT),
	    DCN_SURFACE_FLIP_CLEAR | (flip ? DCN_SURFACE_FLIP_INT_MASK : 0));
}

/* Frames since attach: OTG_STATUS_FRAME_COUNT is 24 bits, widened here. */
static uint64_t
display_frames(struct display_output *o)
{
	uint32_t raw = o_rd(o, OTG(o, regOTG0_OTG_STATUS_FRAME_COUNT)) &
	    DCN_OTG_FRAME_COUNT_MASK;

	o->frames += (raw - o->frame_raw) & DCN_OTG_FRAME_COUNT_MASK;
	o->frame_raw = raw;
	return (o->frames);
}

/*
 * The OTG frame counter restarts when the timing generator is (re)enabled: on
 * a mode set and across an FLR. Bank the frames counted so far, then take the
 * restarted value as the new base, so the counter userland sees stays
 * monotonic and does not jump by 2^24 at each restart.
 */
static void
display_frames_rebase(struct display_output *o)
{
	o->frame_raw = o_rd(o, OTG(o, regOTG0_OTG_STATUS_FRAME_COUNT)) &
	    DCN_OTG_FRAME_COUNT_MASK;
}

static void
display_set_mode(struct display_output *o)
{
	(void)display_frames(o);
	o_wr(o, SIMR(o, regDISP_SET_MODE), 1);
	display_frames_rebase(o);
}

/* Stop OTGn: the pipe goes dark (no scanout, no vblank). Hardware only. */
static void
display_hw_off(struct display_output *o)
{
	(void)display_frames(o);
	display_irqs(o, 0, 0);
	o_wr(o, OTG(o, regOTG0_OTG_CONTROL), 0);
}

/* Signal (and let go of) the outstanding commit's out-fence. */
static void
display_signal_out(struct display_output *o)
{
	struct atrium_amd_softc *sc = o->sc;

	mtx_assert(&sc->lock, MA_OWNED);
	if (o->out_so != NULL && sc->backend != NULL)
		sc->backend->syncobj_signal(o->out_so, o->out_value);
	display_park(sc, o->out_fp);
	o->out_fp = NULL;
	o->out_so = NULL;
}

static void
display_drop_in(struct display_output *o)
{
	struct atrium_amd_softc *sc = o->sc;

	if (o->in_so != NULL && sc->backend != NULL)
		sc->backend->syncobj_watch(o->in_so, NULL, NULL);
	display_park(sc, o->in_fp);
	o->in_fp = NULL;
	o->in_so = NULL;
}

/* The outstanding commit is on screen: it replaces the old on-screen BO, which
 * is released, and the out-fence fires. */
static void
display_landed(struct display_output *o)
{
	mtx_assert(&o->sc->lock, MA_OWNED);
	display_park(o->sc, o->on_screen.bo_fp);
	o->on_screen = o->next;
	o->flips_landed++;
	/* A frame rendered after the last reset repaints the screen. */
	if (o->on_screen.reset_gen == o->sc->reset_gen)
		o->content_lost = 0;
	memset(&o->next, 0, sizeof(o->next));
	o->next_state = 0;
	display_drop_in(o);
	display_signal_out(o);
}

/* The outstanding commit won't reach the screen (the hardware refused it, or
 * its holder went away before its fence): drop it, keep the old buffer up,
 * and still signal the out-fence so nothing waits forever. */
static void
display_abandon(struct display_output *o)
{
	mtx_assert(&o->sc->lock, MA_OWNED);
	display_park(o->sc, o->next.bo_fp);
	memset(&o->next, 0, sizeof(o->next));
	o->next_state = 0;
	display_drop_in(o);
	display_signal_out(o);
}

/* Has the flip we programmed landed? HUBPn holds SURFACE_FLIP_PENDING until
 * the requested surface is the one being scanned out. */
static void
display_reap(struct display_output *o)
{
	mtx_assert(&o->sc->lock, MA_OWNED);
	if (o->next_state != 2)
		return;
	if ((o_rd(o, HUBP(o, regHUBPREQ0_DCSURF_FLIP_CONTROL)) &
	    DCN_SURFACE_FLIP_PENDING) == 0)
		display_landed(o);
}

/* Program the outstanding commit's flip. Returns the referee fault (0 = ok);
 * on a fault nothing was queued. */
static uint32_t
display_flip_next(struct display_output *o)
{
	uint32_t fault;

	mtx_assert(&o->sc->lock, MA_OWNED);
	/* Lock, program, unlock: the unlock is the flip. */
	display_program_surface(o, &o->next, o->next_vsync);
	display_unlock(o, o->next_vsync);
	fault = o_rd(o, SIMR(o, regDISP_FAULT));
	if (fault != 0)
		return (fault);
	o->next_state = 2;
	display_drop_in(o);	/* the fence has done its job */
	display_reap(o);	/* an immediate flip has already landed */
	return (0);
}

static int
display_fence_ready(struct atrium_amd_softc *sc, struct atrium_amd_syncobj *so,
    uint64_t value)
{
	if (so == NULL)
		return (1);
	/* 1 reached, 0 not yet, -errno: the render will never finish. A
	 * syncobj only exists while the gpu module (and its vtable) does. */
	return (sc->backend->syncobj_ready(so, value));
}

/*
 * A commit waiting on its in-fence flips as soon as the fence signals: the
 * syncobj's watch (display_fence_kick) queues the output's fence_task, which
 * runs this. Vblank and the next commit retry it too, as a backstop. An
 * in-fence that fails (its render was lost to a hang, fault or reset) drops
 * the commit: the old buffer stays up and the out-fence fails with the same
 * error, so the holder knows the frame never showed (GPU ABI v2.1 §10.3).
 */
static void
display_try_fence(struct display_output *o)
{
	struct atrium_amd_softc *sc = o->sc;
	uint32_t fault;
	int ready;

	mtx_assert(&sc->lock, MA_OWNED);
	if (o->next_state != 1)
		return;
	ready = display_fence_ready(sc, o->in_so, o->in_value);
	if (ready < 0) {
		if (o->out_so != NULL && sc->backend != NULL)
			sc->backend->syncobj_fail(o->out_so, -ready);
		display_park(sc, o->out_fp);
		o->out_fp = NULL;
		o->out_so = NULL;
		display_abandon(o);
		return;
	}
	if (ready == 0)
		return;
	fault = display_flip_next(o);
	if (fault != 0) {
		o->last_fault = fault;
		display_abandon(o);
	}
}

/* The in-fence's watch: called under the syncobj's lock, so only queue. */
static void
display_fence_kick(void *arg)
{
	struct display_output *o = arg;

	taskqueue_enqueue(taskqueue_thread, &o->fence_task);
}

static void
display_fence_task(void *arg, int pending __unused)
{
	struct display_output *o = arg;

	mtx_lock(&o->sc->lock);
	display_try_fence(o);
	mtx_unlock(&o->sc->lock);
	display_drain(o->sc, curthread);
}

/* IH FLIP cause: flips reached the screen, on the pipes the ISR tallied.
 * sc->lock held. */
static void
amd_display_flip_handler(struct atrium_amd_softc *sc, int count __unused)
{
	struct display_output *o;
	uint32_t n;

	for (n = 0; n < DISPLAY_NOUT; n++) {
		if (sc->ih_disp_count[ATRIUM_AMD_IH_CAUSE_FLIP][n] == 0)
			continue;
		o = display_out(sc, n);
		/* Acknowledge (FLIP_CLEAR), keeping the interrupt enabled. */
		o_wr(o, HUBP(o, regHUBPREQ0_DCSURF_SURFACE_FLIP_INTERRUPT),
		    DCN_SURFACE_FLIP_CLEAR | DCN_SURFACE_FLIP_INT_MASK);
		display_reap(o);
	}
}

/*
 * Does a scanout buffer cover a width x height XRGB8888 mode? The pitch must
 * hold a full row and pitch x height must fit in the BO, or the display engine
 * would read past the BO into whatever VRAM follows it (isolation break 4).
 */
static int
display_fb_fits(const struct display_fb *fb, uint32_t width, uint32_t height)
{
	uint64_t pitch = fb->pitch != 0 ? fb->pitch : (uint64_t)width * 4;

	/* pitch 0 means packed (width * 4), as the ABI defines it. */
	if (pitch < (uint64_t)width * 4)
		return (0);
	return (pitch * height <= fb->size);
}

/*
 * --- commits -----------------------------------------------------------------
 *
 * Every commit, the 2.x COMMIT on output 0 and each element of a 3.0
 * COMMIT_MULTI, is first prepared without the lock (display_prepare: the
 * struct checked, the fds resolved, the mode picked from the monitor's list
 * and the buffer checked against it), then applied under it: a flip by
 * display_queue_flip, a mode set or turn-off by display_topology.
 */
struct display_req {
	struct display_output	*o;
	struct atrium_display_commit *c;
	uint32_t		 flags;
	int			 off;		/* COMMIT_OFF */
	struct display_fb	 fb;		/* bo_fp NULL with off */
	struct file		*in_fp, *out_fp;
	struct atrium_amd_syncobj *in_so, *out_so;
	struct atrium_display_sink *sink;	/* MODESET: the monitor it lights */
	struct atrium_display_timing tm;
	int			 have_tm;
	uint32_t		 vrr_min_hz;
};

static void
display_req_release(struct display_req *r, struct thread *td)
{
	if (r->out_fp != NULL)
		fdrop(r->out_fp, td);
	if (r->in_fp != NULL)
		fdrop(r->in_fp, td);
	if (r->fb.bo_fp != NULL)
		fdrop(r->fb.bo_fp, td);
	free(r->sink, M_ATRIUM_DISPLAY);	/* free(NULL) is a no-op */
	memset(r, 0, sizeof(*r));
}

/*
 * Check `c` for output `o` and resolve what it names. With `fds_only` (the
 * 2.x TEST_ONLY) stop once the fds resolve and the BO is scannable. Returns
 * an errno; on error nothing is held.
 */
static int
display_prepare(struct display_output *o, struct atrium_display_commit *c,
    int multi, int fds_only, struct display_req *r, struct thread *td)
{
	struct atrium_amd_softc *sc = o->sc;
	const struct atrium_gpu_backend_ops *ops;
	struct atrium_amd_bo *bo;
	uint64_t off, size;
	uint32_t gen, allowed;
	int err;

	memset(r, 0, sizeof(*r));
	r->o = o;
	r->c = c;
	r->flags = c->flags;
	c->out_fault = 0;
	allowed = ATRIUM_DISPLAY_COMMIT_MODESET | ATRIUM_DISPLAY_COMMIT_ASYNC |
	    ATRIUM_DISPLAY_COMMIT_VRR | (multi ? ATRIUM_DISPLAY_COMMIT_OFF :
	    ATRIUM_DISPLAY_COMMIT_TEST_ONLY);
	if (c->struct_size < sizeof(*c) || (c->flags & ~allowed) != 0 ||
	    ((c->flags & ATRIUM_DISPLAY_COMMIT_VRR) != 0 &&
	    (c->flags & ATRIUM_DISPLAY_COMMIT_MODESET) == 0))
		return (EINVAL);
	if (c->flags & ATRIUM_DISPLAY_COMMIT_OFF) {
		/* Nothing to scan out: no buffer, no fences, no mode. */
		if (c->flags != ATRIUM_DISPLAY_COMMIT_OFF || c->bo_fd != -1 ||
		    c->in_fence_fd != -1 || c->out_fence_fd != -1)
			return (EINVAL);
		r->off = 1;
		return (0);
	}
	if ((c->format != ATRIUM_DISPLAY_FORMAT_XRGB8888 &&
	    c->format != ATRIUM_DISPLAY_FORMAT_XRGB2101010) ||
	    c->modifier != ATRIUM_DISPLAY_MODIFIER_LINEAR || c->bo_offset != 0 ||
	    (c->pitch % 256) != 0)
		return (EINVAL);

	mtx_lock(&sc->lock);
	ops = sc->backend;
	mtx_unlock(&sc->lock);
	if (ops == NULL || ops->bo_fget == NULL)
		return (ENXIO);

	err = ops->bo_fget(td, c->bo_fd, &r->fb.bo_fp, &bo);
	if (err != 0)
		return (err);
	if ((err = ops->export_scanout(bo, &off, &size)) != 0)	/* VRAM only */
		goto fail;
	if (c->in_fence_fd >= 0 && (err = ops->syncobj_fget(td,
	    c->in_fence_fd, &r->in_fp, &r->in_so)) != 0)
		goto fail;
	if (c->out_fence_fd >= 0 && (err = ops->syncobj_fget(td,
	    c->out_fence_fd, &r->out_fp, &r->out_so)) != 0)
		goto fail;
	if (fds_only)
		return (0);	/* the fds resolve and the BO is scannable */

	r->fb.reset_gen = sc->reset_gen;	/* unlocked read: a racing reset
						 * only makes this frame count
						 * as stale */
	r->fb.vram_offset = off;
	r->fb.size = size;
	r->fb.pitch = c->pitch;
	r->fb.format = c->format;
	if ((c->flags & ATRIUM_DISPLAY_COMMIT_MODESET) == 0)
		return (0);

	/* The mode comes from the connector's list (probed unlocked). */
	r->sink = malloc(sizeof(*r->sink), M_ATRIUM_DISPLAY, M_WAITOK);
	gen = display_sink_get(o, r->sink, NULL);
	if (c->mode_generation != 0 && c->mode_generation != gen) {
		err = ESTALE;	/* the monitor changed since CONNECTOR */
		goto fail;
	}
	if (r->sink->ntimings > 0) {
		if (c->mode_index >= r->sink->ntimings) {
			err = EINVAL;
			goto fail;
		}
		r->tm = r->sink->timings[c->mode_index];
		r->have_tm = 1;
		if (c->flags & ATRIUM_DISPLAY_COMMIT_VRR) {
			/* The mode's refresh must sit in the monitor's
			 * range, rounded to whole Hz. */
			uint32_t hz = (atrium_timing_refresh_mhz(&r->tm) +
			    500) / 1000;

			if (amd_display_hw_ops.set_vrr == NULL ||
			    r->sink->vrr_min_hz == 0 ||
			    hz < r->sink->vrr_min_hz ||
			    hz > r->sink->vrr_max_hz) {
				err = EINVAL;
				goto fail;
			}
			r->vrr_min_hz = r->sink->vrr_min_hz;
		}
		/* The kernel checks the buffer covers the mode it will scan
		 * out; gpusim's referee is the second line, not the first
		 * (review §A2.5, isolation break 4). */
		if (!display_fb_fits(&r->fb, r->tm.hactive, r->tm.vactive)) {
			err = EINVAL;
			goto fail;
		}
	} else if (c->mode_index != 0 ||
	    (c->flags & ATRIUM_DISPLAY_COMMIT_VRR) != 0) {
		err = EINVAL;	/* no monitor: nothing to index */
		goto fail;
	}
	return (0);
fail:
	display_req_release(r, td);
	return (err);
}

/*
 * Queue `r`'s flip on its output (lock held, the output free). Returns the
 * referee fault if the hardware refused it at once (the request keeps its
 * references and nothing changed); 0 when the output owns them now.
 */
static uint32_t
display_queue_flip(struct display_req *r)
{
	struct display_output *o = r->o;
	struct atrium_amd_softc *sc = o->sc;
	struct atrium_display_commit *c = r->c;
	uint32_t fault;

	mtx_assert(&sc->lock, MA_OWNED);
	o->next = r->fb;
	o->next_vsync = (c->flags & ATRIUM_DISPLAY_COMMIT_ASYNC) == 0;
	o->in_fp = r->in_fp;
	o->in_so = r->in_so;
	o->in_value = c->in_fence_value;
	o->out_fp = r->out_fp;
	o->out_so = r->out_so;
	o->out_value = c->out_fence_value;
	o->next_state = 1;
	/* Flip the moment the render signals (display_fence_kick), installed
	 * before the check so a signal in between is not missed. */
	if (r->in_so != NULL)
		sc->backend->syncobj_watch(r->in_so, display_fence_kick, o);
	if (display_fence_ready(sc, r->in_so, c->in_fence_value) > 0) {
		fault = display_flip_next(o);
		if (fault != 0) {
			/* Refused now: report it to the caller, keep its
			 * references for it to release, and leave the screen
			 * as it was. */
			memset(&o->next, 0, sizeof(o->next));
			o->next_state = 0;
			if (r->in_so != NULL)
				sc->backend->syncobj_watch(r->in_so, NULL, NULL);
			o->in_fp = NULL;
			o->in_so = NULL;
			o->out_fp = NULL;
			o->out_so = NULL;
			return (fault);
		}
	} else
		display_try_fence(o);	/* a failed in-fence drops it now */
	if (o->hdr_pending_valid)
		display_hdr_commit(o, NULL);
	/* The output owns the references now. */
	r->fb.bo_fp = NULL;
	r->in_fp = r->out_fp = NULL;
	return (0);
}

/* Can `r`'s flip go up on its output as it is now? An errno, or a fault in
 * *fault (0 = fine). Lock held. */
static int
display_flip_check(struct display_req *r, uint32_t *fault)
{
	struct display_output *o = r->o;

	mtx_assert(&o->sc->lock, MA_OWNED);
	*fault = 0;
	display_reap(o);
	if (o->next_state != 0)
		return (EBUSY);
	if (o->mode_set && o->mode_width != 0 &&
	    !display_fb_fits(&r->fb, o->mode_width, o->mode_height))
		return (EINVAL);	/* would scan out past the end of the BO */
	if (!o->mode_set)
		*fault = ATRIUM_DISPLAY_FAULT_NO_MODE;
	return (0);
}

/* What `o` keeps of its lit state, to put it back if a topology commit
 * fails part way. */
struct display_snap {
	int			 mode_set;
	struct display_fb	 on_screen;
	struct atrium_display_timing timing;
	int			 timing_valid;
	uint32_t		 vrr_min_hz;
	uint32_t		 width;
};

/* Light `o` at `tm` (if any) with `fb` up from the first frame. Returns the
 * referee fault. Lock held. */
static uint32_t
display_hw_light(struct display_output *o, const struct display_fb *fb,
    const struct atrium_display_timing *tm, uint32_t vrr_min_hz,
    const struct atrium_display_hdr *hdr)
{
	uint32_t fault;

	if (tm != NULL) {
		o->width = tm->hactive;
		amd_display_hw_ops.program_timing(o, tm);
		if (amd_display_hw_ops.set_vrr != NULL)
			amd_display_hw_ops.set_vrr(o, tm, vrr_min_hz);
	}
	/* The surface goes up with the mode (the SIM mode set takes the held
	 * surface); the unlock after it then has nothing to flip. */
	display_program_surface(o, fb, 1);
	display_set_mode(o);
	display_unlock(o, 1);
	fault = o_rd(o, SIMR(o, regDISP_FAULT));
	if (fault == 0) {
		/* The CRTC is live: arm the vblank and flip-done interrupts.
		 * The device raises them through the IH ring and the base ISR
		 * routes them here, by pipe. */
		display_irqs(o, 1, 1);
		if (hdr != NULL)
			display_hdr_send(o, hdr);
	}
	return (fault);
}

/*
 * Span groups (3.1). The hardware's half: the pipe joins its timing group
 * (DCN OTG timing synchronisation; on gpusim the SIM SYNC_GROUP register,
 * where the group's master clock drives every member) and arms its flip
 * lock. A mode set or OTG disable takes the pipe out in hardware, so this
 * is re-run after anything that re-lights a member. A fault, or 0.
 */
static uint32_t
display_sync_hw(struct display_output *o)
{
	uint32_t fault;

	if (o->sync_group == 0)
		o->sync_vrr_hz = 0;
	/* One frame period for the group: every VRR member runs the range
	 * all of them can take. */
	if (o->vrr_min_hz != 0 && o->timing_valid &&
	    amd_display_hw_ops.set_vrr != NULL)
		amd_display_hw_ops.set_vrr(o, &o->timing,
		    o->sync_vrr_hz != 0 ? o->sync_vrr_hz : o->vrr_min_hz);
	o_wr(o, SIMR(o, regDISP_SYNC_GROUP), o->sync_group);
	if (o->sync_group != 0) {
		fault = o_rd(o, SIMR(o, regDISP_FAULT));
		if (fault != 0)
			return (fault);
	}
	o_wr(o, SIMR(o, regDISP_FLIP_LOCK), o->sync_group != 0 && o->flip_lock);
	return (0);
}

/* Put every group back in the hardware (after a reset, or a topology
 * commit that was rolled back). Lock held. */
static void
display_sync_reapply(struct atrium_amd_softc *sc)
{
	struct display_output *o;
	uint32_t n;

	mtx_assert(&sc->lock, MA_OWNED);
	for (n = 0; n < DISPLAY_NOUT; n++) {
		o = display_out(sc, n);
		if (o->sync_group != 0 && o->mode_set)
			(void)display_sync_hw(o);
	}
}

/* The outputs whose flips must go up together with `o`'s: its group's
 * flip-locked members (0 when `o` itself isn't locked). Lock held. */
static uint32_t
display_lock_set(struct atrium_amd_softc *sc, struct display_output *o)
{
	struct display_output *m;
	uint32_t n, set = 0;

	if (o->sync_group == 0 || !o->flip_lock)
		return (0);
	for (n = 0; n < DISPLAY_NOUT; n++) {
		m = display_out(sc, n);
		if (m->sync_group == o->sync_group && m->flip_lock)
			set |= 1u << n;
	}
	return (set);
}

/* COMMIT_SYNC_GROUP (3.1): set group g->group's members and flip lock. */
static int
display_commit_sync_group(struct atrium_amd_softc *sc,
    struct atrium_display_sync_group *g)
{
	struct display_output *o, *first = NULL;
	uint32_t n, bit, all = (1u << DISPLAY_NOUT) - 1, fault = 0, vrr_hz = 0;

	if (g->struct_size < sizeof(*g) || g->flags != 0 || g->_pad != 0 ||
	    g->group == 0 || g->group > ATRIUM_DISPLAY_SYNC_GROUPS ||
	    (g->members & ~all) != 0 || (g->flip_lock & ~g->members) != 0)
		return (EINVAL);
	g->out_failed = ATRIUM_DISPLAY_MULTI_NONE;
	g->out_reason = 0;
	mtx_lock(&sc->lock);
	/* Check: every member lit, all at one timing, and variable refresh on
	 * all of them or none (the group runs the highest floor). */
	for (n = 0; n < DISPLAY_NOUT; n++) {
		if ((g->members & (1u << n)) == 0)
			continue;
		o = display_out(sc, n);
		if (!o->mode_set || !o->timing_valid)
			fault = ATRIUM_DISPLAY_FAULT_NO_MODE;
		else if (first != NULL &&
		    (!atrium_timing_equal(&o->timing, &first->timing) ||
		    (o->vrr_min_hz == 0) != (first->vrr_min_hz == 0)))
			fault = ATRIUM_DISPLAY_FAULT_SYNC_MISMATCH;
		else if (first == NULL)
			first = o;
		if (fault == 0 && o->vrr_min_hz > vrr_hz)
			vrr_hz = o->vrr_min_hz;
		if (fault != 0) {
			g->out_failed = n;
			g->out_reason = fault;
			mtx_unlock(&sc->lock);
			return (0);
		}
	}
	/* Apply: members leaving first, then the members in pipe order (the
	 * lowest becomes the master the others lock to). A new VRR floor
	 * takes the whole group out first: the hardware checks a member
	 * joining against those already in. */
	for (n = 0; n < DISPLAY_NOUT; n++) {
		o = display_out(sc, n);
		if (o->sync_group == g->group && (g->members & (1u << n)) == 0) {
			o->sync_group = 0;
			o->flip_lock = 0;
			(void)display_sync_hw(o);
		} else if (o->sync_group == g->group && o->sync_vrr_hz != vrr_hz)
			o_wr(o, SIMR(o, regDISP_SYNC_GROUP), 0);
	}
	for (n = 0; n < DISPLAY_NOUT && fault == 0; n++) {
		bit = 1u << n;
		if ((g->members & bit) == 0)
			continue;
		o = display_out(sc, n);
		o->sync_group = g->group;
		o->flip_lock = (g->flip_lock & bit) != 0;
		o->sync_vrr_hz = vrr_hz;
		fault = display_sync_hw(o);
		if (fault != 0) {
			g->out_failed = n;
			g->out_reason = fault;
		}
	}
	if (fault != 0) {
		/* The hardware refused what the check passed: dissolve. */
		for (n = 0; n < DISPLAY_NOUT; n++) {
			o = display_out(sc, n);
			if (o->sync_group == g->group) {
				o->sync_group = 0;
				o->flip_lock = 0;
				(void)display_sync_hw(o);
			}
		}
	}
	mtx_unlock(&sc->lock);
	return (0);
}

/* The pixel rate `o` draws while lit, kHz. Lock held. */
static uint32_t
display_khz(struct display_output *o)
{
	return (o->mode_set && o->timing_valid ? o->timing.pixel_clock_khz : 0);
}

/*
 * A topology commit: `n` requests, each a MODESET or an OFF on a distinct
 * output. Atomic: every check runs before any register is written, and if
 * the hardware still refuses an output the ones already changed are put
 * back. Returns an errno, or 0 with *failed / *reason set (NONE / 0 when it
 * went through). With `test` nothing is applied. Requests whose references
 * the outputs took have them cleared; the caller releases the rest.
 */
static int
display_topology(struct atrium_amd_softc *sc, struct display_req *rq,
    uint32_t n, int test, uint32_t *failed, uint32_t *reason)
{
	struct display_snap snap[DISPLAY_NOUT];
	struct display_output *o;
	struct display_req *r;
	uint64_t total;
	uint32_t i, k, budget, fault, touched = 0;
	int ready;

	*failed = ATRIUM_DISPLAY_MULTI_NONE;
	*reason = 0;
	mtx_lock(&sc->lock);
	for (i = 0; i < n; i++) {
		o = rq[i].o;
		touched |= 1u << o->n;
		display_reap(o);
		if (o->next_state != 0) {
			mtx_unlock(&sc->lock);
			return (EBUSY);
		}
		/* A mode set applies at once: its frame must be rendered. */
		ready = display_fence_ready(sc, rq[i].in_so,
		    rq[i].c->in_fence_value);
		if (ready <= 0) {
			mtx_unlock(&sc->lock);
			return (ready < 0 ? -ready : EAGAIN);
		}
	}
	/*
	 * Bandwidth: what the outputs it leaves alone draw, then each lit
	 * output it asks for in turn. The first that takes the total past the
	 * budget is the one that cannot have its part.
	 */
	budget = amd_mmio_read32(sc, regDISP_BW_BUDGET_KHZ);
	total = 0;
	for (k = 0; k < DISPLAY_NOUT; k++)
		if ((touched & (1u << k)) == 0)
			total += display_khz(display_out(sc, k));
	for (i = 0; i < n; i++) {
		if (rq[i].off || !rq[i].have_tm)
			continue;
		total += rq[i].tm.pixel_clock_khz;
		if (budget != 0 && total > budget) {
			*failed = i;
			*reason = ATRIUM_DISPLAY_FAULT_BANDWIDTH;
			break;
		}
	}
	if (test || *failed != ATRIUM_DISPLAY_MULTI_NONE) {
		mtx_unlock(&sc->lock);
		return (0);
	}

	/*
	 * Apply. Every output it names goes dark first, so the pipes light up
	 * again with the bandwidth only ever growing towards the checked total;
	 * then each MODESET lights its output.
	 */
	for (i = 0; i < n; i++) {
		o = rq[i].o;
		snap[i].mode_set = o->mode_set;
		snap[i].on_screen = o->on_screen;
		snap[i].timing = o->timing;
		snap[i].timing_valid = o->timing_valid;
		snap[i].vrr_min_hz = o->vrr_min_hz;
		snap[i].width = o->width;
		if (o->mode_set)
			display_hw_off(o);
	}
	fault = 0;
	for (i = 0; i < n; i++) {
		r = &rq[i];
		if (r->off)
			continue;
		fault = display_hw_light(r->o, &r->fb, r->have_tm ? &r->tm :
		    NULL, r->vrr_min_hz, NULL);
		if (fault != 0)
			break;
	}
	if (fault != 0) {
		/* Put every output back as it was. */
		*failed = i;
		*reason = fault;
		for (k = 0; k <= i; k++)
			if (!rq[k].off)
				display_hw_off(rq[k].o);
		for (k = 0; k < n; k++) {
			o = rq[k].o;
			o->width = snap[k].width;
			if (!snap[k].mode_set || snap[k].on_screen.bo_fp == NULL)
				continue;
			(void)display_hw_light(o, &snap[k].on_screen,
			    snap[k].timing_valid ? &snap[k].timing : NULL,
			    snap[k].vrr_min_hz, &o->hdr);
		}
		display_sync_reapply(sc);
		mtx_unlock(&sc->lock);
		return (0);
	}

	/* It went through: the outputs take their new state. */
	for (i = 0; i < n; i++) {
		r = &rq[i];
		o = r->o;
		display_park(sc, o->on_screen.bo_fp);
		/* Re-moded or off: its timing generator restarted, so it is
		 * out of its sync group (COMMIT_SYNC_GROUP puts it back). */
		o->sync_group = 0;
		o->flip_lock = 0;
		if (r->off) {
			memset(&o->on_screen, 0, sizeof(o->on_screen));
			o->mode_set = 0;
			o->timing_valid = 0;
			o->vrr_min_hz = 0;
			o->refresh_mhz = 0;
			o->mode_width = o->mode_height = 0;
			continue;
		}
		o->mode_set = 1;
		o->timing_valid = r->have_tm;
		if (r->have_tm)
			o->timing = r->tm;
		o->vrr_min_hz = r->have_tm ? r->vrr_min_hz : 0;
		display_hdr_commit(o, r->sink);
		o->refresh_mhz = r->have_tm ? atrium_timing_refresh_mhz(&r->tm) : 0;
		o->mode_width = r->have_tm ? r->tm.hactive : 0;
		o->mode_height = r->have_tm ? r->tm.vactive : 0;
		/* A modeset scans the new buffer out from its first frame. */
		o->on_screen = r->fb;
		if (o->on_screen.reset_gen == sc->reset_gen)
			o->content_lost = 0;
		if (r->out_so != NULL)
			sc->backend->syncobj_signal(r->out_so,
			    r->c->out_fence_value);
		display_park(sc, r->out_fp);
		display_park(sc, r->in_fp);
		r->fb.bo_fp = NULL;
		r->in_fp = r->out_fp = NULL;
	}
	mtx_unlock(&sc->lock);
	return (0);
}

/* COMMIT (2.x): one commit on output 0. */
static int
display_commit(struct atrium_amd_softc *sc, struct atrium_display_commit *c,
    struct thread *td)
{
	struct display_output *o = display_out(sc, 0);
	struct display_req r;
	uint32_t failed, reason, fault;
	int err;

	display_drain(sc, td);
	err = display_prepare(o, c, 0,
	    (c->flags & ATRIUM_DISPLAY_COMMIT_TEST_ONLY) != 0, &r, td);
	if (err != 0)
		return (err);
	if (c->flags & ATRIUM_DISPLAY_COMMIT_TEST_ONLY) {
		display_req_release(&r, td);
		return (0);
	}
	if (c->flags & ATRIUM_DISPLAY_COMMIT_MODESET) {
		err = display_topology(sc, &r, 1, 0, &failed, &reason);
		if (err == 0 && failed != ATRIUM_DISPLAY_MULTI_NONE)
			c->out_fault = reason;	/* a referee fault, not an errno */
	} else {
		mtx_lock(&sc->lock);
		/* A flip-locked output flips with its group: COMMIT_MULTI. */
		err = (display_lock_set(sc, o) & ~1u) != 0 ? EINVAL :
		    display_flip_check(&r, &fault);
		if (err == 0 && fault == 0)
			fault = display_queue_flip(&r);
		mtx_unlock(&sc->lock);
		if (err == 0)
			c->out_fault = fault;
	}
	display_req_release(&r, td);
	display_drain(sc, td);
	return (err);
}

/* COMMIT_MULTI (3.0): several outputs, atomically for a topology change. */
static int
display_commit_multi(struct atrium_amd_softc *sc,
    struct atrium_display_commit_multi *m, struct thread *td)
{
	struct atrium_display_output_commit *oc;
	struct display_req *rq;
	uint32_t i, k, seen = 0, topo = 0, fault;
	int err, test;

	if (m->struct_size < sizeof(*m) ||
	    (m->flags & ~ATRIUM_DISPLAY_COMMIT_TEST_ONLY) != 0 ||
	    m->count == 0 || m->count > ATRIUM_DISPLAY_MAX_OUTPUTS ||
	    m->_pad != 0)
		return (EINVAL);
	test = (m->flags & ATRIUM_DISPLAY_COMMIT_TEST_ONLY) != 0;
	m->out_failed = ATRIUM_DISPLAY_MULTI_NONE;
	m->out_reason = 0;
	oc = malloc(sizeof(*oc) * m->count, M_ATRIUM_DISPLAY, M_WAITOK);
	rq = malloc(sizeof(*rq) * m->count, M_ATRIUM_DISPLAY,
	    M_WAITOK | M_ZERO);
	err = copyin((const void *)(uintptr_t)m->commits, oc,
	    sizeof(*oc) * m->count);
	if (err != 0)
		goto done;
	for (i = 0; i < m->count; i++) {
		if (oc[i]._pad != 0) {
			err = EINVAL;
			goto done;
		}
		if (oc[i].c.flags & (ATRIUM_DISPLAY_COMMIT_MODESET |
		    ATRIUM_DISPLAY_COMMIT_OFF))
			topo++;
	}
	if (topo != 0 && topo != m->count) {
		err = EINVAL;	/* a topology change or flips, not both */
		goto done;
	}
	/* Each output once, and only outputs that exist. */
	for (i = 0; i < m->count; i++) {
		oc[i].c.out_fault = 0;
		if (oc[i].output >= DISPLAY_NOUT ||
		    (seen & (1u << oc[i].output)) != 0) {
			m->out_failed = i;
			m->out_reason = oc[i].c.out_fault =
			    ATRIUM_DISPLAY_FAULT_NO_OUTPUT;
			goto copyout;
		}
		seen |= 1u << oc[i].output;
	}

	display_drain(sc, td);
	for (i = 0; i < m->count; i++) {
		err = display_prepare(display_out(sc, oc[i].output), &oc[i].c, 1,
		    0, &rq[i], td);
		if (err != 0)
			goto done;
	}

	if (topo != 0) {
		err = display_topology(sc, rq, m->count, test, &m->out_failed,
		    &m->out_reason);
	} else {
		/* Flips: every output free and every buffer fitting its mode
		 * before any is queued. */
		mtx_lock(&sc->lock);
		/* A span group flips as one: naming one flip-locked output
		 * means naming all of its group's. */
		for (i = 0; i < m->count && err == 0; i++)
			if ((display_lock_set(sc, rq[i].o) & ~seen) != 0)
				err = EINVAL;
		for (i = 0; i < m->count && err == 0; i++) {
			err = display_flip_check(&rq[i], &fault);
			if (err == 0 && fault != 0 &&
			    m->out_failed == ATRIUM_DISPLAY_MULTI_NONE) {
				m->out_failed = i;
				m->out_reason = fault;
			}
		}
		for (i = 0; err == 0 && !test &&
		    m->out_failed == ATRIUM_DISPLAY_MULTI_NONE &&
		    i < m->count; i++) {
			fault = display_queue_flip(&rq[i]);
			if (fault != 0) {
				m->out_failed = i;
				m->out_reason = fault;
			}
		}
		mtx_unlock(&sc->lock);
	}
	if (err == 0 && m->out_failed != ATRIUM_DISPLAY_MULTI_NONE)
		oc[m->out_failed].c.out_fault = m->out_reason;
copyout:
	if (err == 0)
		err = copyout(oc, (void *)(uintptr_t)m->commits,
		    sizeof(*oc) * m->count);
done:
	for (k = 0; k < m->count; k++)
		display_req_release(&rq[k], td);
	free(rq, M_ATRIUM_DISPLAY);
	free(oc, M_ATRIUM_DISPLAY);
	display_drain(sc, td);
	return (err);
}

/* STATUS_V2 for output `o`. */
static int
display_status(struct display_output *o, struct atrium_display_status_v2 *st)
{
	struct atrium_amd_softc *sc = o->sc;

	if (st->struct_size < sizeof(*st))
		return (EINVAL);
	mtx_lock(&sc->lock);
	display_reap(o);	/* also the poll-mode (no MSI-X) path */
	st->out_flags = (o->next_state != 0 ?
	    ATRIUM_DISPLAY_STATUS_COMMIT_PENDING : 0) |
	    (o->mode_set ? ATRIUM_DISPLAY_STATUS_MODE_SET : 0) |
	    (o->content_lost ? ATRIUM_DISPLAY_STATUS_CONTENT_LOST : 0);
	st->out_vblank_count = display_frames(o);
	st->out_flips_done = o->flips_landed;
	st->out_dropped_flips = o_rd(o, SIMR(o, regDISP_DROPPED_FLIPS));
	st->out_tear_line = o_rd(o, SIMR(o, regDISP_TEAR_LINE));
	st->out_refresh_mhz = o->refresh_mhz;
	st->out_last_fault = o->last_fault;
	mtx_unlock(&sc->lock);
	return (0);
}

/* OUTPUTS (3.0): every output at a glance. */
static int
display_outputs(struct atrium_amd_softc *sc, struct atrium_display_outputs *v)
{
	struct atrium_display_sink *sink;
	struct atrium_display_output_info *in;
	struct display_output *o;
	uint64_t id;
	uint32_t n, used = 0;

	if (v->struct_size < sizeof(*v))
		return (EINVAL);
	memset(v->out, 0, sizeof(v->out));
	sink = malloc(sizeof(*sink), M_ATRIUM_DISPLAY, M_WAITOK);
	for (n = 0; n < DISPLAY_NOUT; n++) {
		o = display_out(sc, n);
		in = &v->out[n];
		in->generation = display_sink_get(o, sink, &id);
		in->identity = id;
		in->connector_type = o_rd(o, SIMR(o, regDISP_CONNECTOR_TYPE));
		mtx_lock(&sc->lock);
		in->flags = display_conn_flags(o, sink, o->mode_set &&
		    o->timing_valid);
		if (o->mode_set) {
			in->flags |= ATRIUM_DISPLAY_OUTPUT_LIT;
			in->width = o->mode_width;
			in->height = o->mode_height;
			in->refresh_mhz = o->refresh_mhz;
			in->pixel_clock_khz = display_khz(o);
			used += in->pixel_clock_khz;
		}
		in->sync_group = o->sync_group;
		if (o->sync_group != 0 && o->flip_lock)
			in->flags |= ATRIUM_DISPLAY_OUTPUT_FLIP_LOCKED;
		if (o->sync_group != 0 &&
		    o_rd(o, SIMR(o, regDISP_SYNC_MASTER)) != 0)
			in->flags |= ATRIUM_DISPLAY_OUTPUT_SYNC_MASTER;
		mtx_unlock(&sc->lock);
	}
	free(sink, M_ATRIUM_DISPLAY);
	mtx_lock(&sc->lock);
	v->out_topology_generation = sc->display_state->topo_gen;
	mtx_unlock(&sc->lock);
	v->out_count = DISPLAY_NOUT;
	v->out_bandwidth_khz = amd_mmio_read32(sc, regDISP_BW_BUDGET_KHZ);
	v->out_bandwidth_used_khz = used;
	v->_pad = 0;
	return (0);
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

/* Plug simulated monitor `mode` (the CONFIG plug_mode codes; -1 unplugs)
 * into output `o`. */
static int
display_sim_plug(struct display_output *o, int mode)
{
	if (mode == -1)
		o_wr(o, SIMR(o, regDISP_CFG_UNPLUG), 1);
	else if (mode >= 0 && mode <= 6)
		o_wr(o, SIMR(o, regDISP_CFG_PLUG_MODE), mode);
	else
		return (EINVAL);
	display_sink_invalidate(o);
	return (0);
}

/*
 * dev.atrium_gpu_amd_display.N.sim_plug: plug a simulated monitor into
 * output 0 (the CONFIG plug_mode codes; -1 unplugs) from outside the lease.
 * A real cable is plugged by someone other than the display's holder, so the
 * hotplug path is tested the same way: frescod keeps the display open while
 * the host swaps the monitor under it. Host root only (no CTLFLAG_PRISON),
 * and it drives the simulator's SIM registers, which silicon does not have.
 */
static int
display_sim_plug_sysctl(SYSCTL_HANDLER_ARGS)
{
	struct atrium_amd_softc *sc = arg1;
	int mode = 0, error;

	error = sysctl_handle_int(oidp, &mode, 0, req);
	if (error != 0 || req->newptr == NULL)
		return (error);
	return (display_sim_plug(display_out(sc, 0), mode));
}

/* dev.atrium_gpu_amd_display.N.sim_plug_at: "OUTPUT:MODE", the same for any
 * output. Reads back "" (write-only in effect). */
static int
display_sim_plug_at_sysctl(SYSCTL_HANDLER_ARGS)
{
	struct atrium_amd_softc *sc = arg1;
	struct display_output *o;
	char buf[24], *colon, *end;
	long n, mode;
	int error;

	buf[0] = '\0';
	error = sysctl_handle_string(oidp, buf, sizeof(buf), req);
	if (error != 0 || req->newptr == NULL)
		return (error);
	colon = strchr(buf, ':');
	if (colon == NULL)
		return (EINVAL);
	*colon = '\0';
	n = strtol(buf, &end, 10);
	if (end == buf || *end != '\0' || n < 0 ||
	    (o = display_out(sc, (uint32_t)n)) == NULL)
		return (EINVAL);
	mode = strtol(colon + 1, &end, 10);
	if (end == colon + 1 || *end != '\0')
		return (EINVAL);
	return (display_sim_plug(o, (int)mode));
}

/*
 * dev.atrium_gpu_amd_display.N.sim_scanout: what every pipe scans out, in one
 * consistent snapshot: "BASE0 BASE1 BASE2 BASE3" (hex VRAM addresses, 0 =
 * dark). Read again if a vblank came in between, so a span test can tell
 * whether a group's members ever show different frames (test readback; the
 * simulator's SCANOUT_BASE registers).
 */
static int
display_sim_scanout_sysctl(SYSCTL_HANDLER_ARGS)
{
	struct atrium_amd_softc *sc = arg1;
	struct display_output *o;
	uint32_t before[DISPLAY_NOUT], n, try;
	uint64_t base[DISPLAY_NOUT];
	char buf[DISPLAY_NOUT * 20];
	int len = 0;

	for (try = 0; try < 16; try++) {
		for (n = 0; n < DISPLAY_NOUT; n++) {
			o = display_out(sc, n);
			before[n] = o_rd(o, SIMR(o, regDISP_VBLANK_COUNT));
		}
		for (n = 0; n < DISPLAY_NOUT; n++) {
			o = display_out(sc, n);
			base[n] = o_rd(o, SIMR(o, regDISP_SCANOUT_BASE_LO)) |
			    (uint64_t)o_rd(o, SIMR(o, regDISP_SCANOUT_BASE_HI)) << 32;
		}
		for (n = 0; n < DISPLAY_NOUT; n++) {
			o = display_out(sc, n);
			if (o_rd(o, SIMR(o, regDISP_VBLANK_COUNT)) != before[n])
				break;
		}
		if (n == DISPLAY_NOUT)
			break;
	}
	if (try == 16)
		return (EAGAIN);
	for (n = 0; n < DISPLAY_NOUT; n++)
		len += snprintf(buf + len, sizeof(buf) - len, "%s%jx",
		    n == 0 ? "" : " ", (uintmax_t)base[n]);
	return (sysctl_handle_string(oidp, buf, sizeof(buf), req));
}

/*
 * dev.atrium_gpu_amd_display.N.sim_sink: what the simulated monitor on
 * output 0 makes of the stream, "eotf=E bt2020=B cll=C fall=F bpc=D" (test
 * readback of the infoframes it decoded; silicon has no such register: a
 * real monitor shows it in its menu).
 */
static int
display_sim_sink_sysctl(SYSCTL_HANDLER_ARGS)
{
	struct atrium_amd_softc *sc = arg1;
	char buf[80];
	uint32_t light;

	light = amd_mmio_read32(sc, regDISP_SINK_CONTENT_LIGHT);
	snprintf(buf, sizeof(buf), "eotf=%u bt2020=%u cll=%u fall=%u bpc=%u",
	    amd_mmio_read32(sc, regDISP_SINK_EOTF),
	    amd_mmio_read32(sc, regDISP_SINK_COLORIMETRY), light & 0xffff,
	    light >> 16, amd_mmio_read32(sc, regDISP_SINK_BPC));
	return (sysctl_handle_string(oidp, buf, sizeof(buf), req));
}

/* dev.atrium_gpu_amd_display.N.sim_bw_budget_khz: the simulator's shared
 * pixel-rate budget (tests lower it to exercise FAULT_BANDWIDTH). */
static int
display_sim_bw_sysctl(SYSCTL_HANDLER_ARGS)
{
	struct atrium_amd_softc *sc = arg1;
	int khz, error;

	khz = (int)amd_mmio_read32(sc, regDISP_BW_BUDGET_KHZ);
	error = sysctl_handle_int(oidp, &khz, 0, req);
	if (error != 0 || req->newptr == NULL)
		return (error);
	if (khz <= 0)
		return (EINVAL);
	amd_mmio_write32(sc, regDISP_BW_BUDGET_KHZ, (uint32_t)khz);
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
	struct display_output *o;
	uint32_t n;

	/*
	 * Hold the last frame: the on-screen buffers (and flips already handed
	 * to the hardware) stay put until the next holder commits. Only commits
	 * still waiting on their in-fence are dropped; their holder is gone.
	 */
	mtx_lock(&sc->lock);
	for (n = 0; n < DISPLAY_NOUT; n++) {
		o = display_out(sc, n);
		if (o->next_state == 1)
			display_abandon(o);
	}
	sc->display_opened = 0;
	mtx_unlock(&sc->lock);
	display_drain(sc, td);
	return (0);
}

/* --- kqueue: EVFILT_READ on /dev/atrium-display0 fires once per vblank ----- */

/*
 * The knote list lives in the shared softc (sc->display_sel) and is fired by the
 * base module's IH ISR through our handlers. We just register/remove knotes on
 * it here; sc->lock (the knlist's lock) guards the list. f_event mirrors
 * EVFILT_TIMER: each vblank of a selected output bumps kn_data, and the filter
 * is ready while kn_data != 0. EV_CLEAR (forced below) makes each kevent()
 * return the vblanks elapsed since the last and reset — the natural "wait for
 * the next vblank(s)" edge semantics, retiring WAIT_VBLANK.
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
	uint32_t outs = kn->kn_sfflags & ATRIUM_DISPLAY_NOTE_OUTPUTS;
	uint32_t bit;

	/* Called under sc->lock (the knlist lock) by KNOTE_LOCKED / the kqueue
	 * scan. hint = DISPLAY_HINT_VBLANK(output, vblanks this interrupt
	 * drained) (0 on a bare readiness scan), DISPLAY_HINT_RESET from the
	 * reset restore hook or DISPLAY_HINT_HOTPLUG from the HPD handler. A
	 * registration with notes in its fflags hears only those, with data =
	 * that event's generation; a vblank registration hears its outputs'
	 * vblanks (output 0 if it names none) and the events (data += 1 for an
	 * event). EV_CLEAR zeroes kn_data and kn_fflags after each delivery. */
	if (outs == 0)
		outs = ATRIUM_DISPLAY_NOTE_OUTPUT(0);
	if (hint == DISPLAY_HINT_RESET || hint == DISPLAY_HINT_HOTPLUG) {
		bit = hint == DISPLAY_HINT_RESET ? ATRIUM_DISPLAY_NOTE_RESET :
		    ATRIUM_DISPLAY_NOTE_HOTPLUG;
		if (notes == 0) {
			kn->kn_fflags |= bit;
			kn->kn_data += 1;
		} else if (notes & bit) {
			kn->kn_fflags |= bit;
			kn->kn_data = hint == DISPLAY_HINT_RESET ?
			    (int64_t)sc->reset_gen :
			    (int64_t)sc->display_state->topo_gen;
		}
	} else if (hint > 0 && notes == 0) {
		bit = ATRIUM_DISPLAY_NOTE_OUTPUT((uint32_t)(hint >> 24) & 7);
		if (outs & bit) {
			kn->kn_fflags |= bit;
			kn->kn_data += hint & 0xffffff;
		}
	}
	if (notes != 0)
		return ((kn->kn_fflags & notes) != 0);
	return (kn->kn_data != 0);
}

/*
 * The IH VBLANK cause handler — registered with the base ISR (amd_ih_set_handler
 * in attach). The base ISR drains the device-global IH ring, tallies each
 * vblank cookie by pipe (sc->ih_disp_count) and calls us with sc->lock HELD.
 * That is exactly KNOTE_LOCKED's contract (the knlist lock IS sc->lock), so we
 * wake the EVFILT_READ waiters on /dev/atrium-display0 directly, once per
 * pipe that had a vblank.
 */
static void
amd_display_vblank_handler(struct atrium_amd_softc *sc, int count __unused)
{
	struct display_output *o;
	uint32_t n, k;

	for (n = 0; n < DISPLAY_NOUT; n++) {
		k = sc->ih_disp_count[ATRIUM_AMD_IH_CAUSE_VBLANK][n];
		if (k == 0)
			continue;
		o = display_out(sc, n);
		/* Acknowledge (VERTICAL_INTERRUPT0_CLEAR), keeping it enabled. */
		o_wr(o, OTG(o, regOTG0_OTG_VERTICAL_INTERRUPT0_CONTROL),
		    DCN_VINT0_CLEAR | DCN_VINT0_INT_ENABLE);
		KNOTE_LOCKED(&sc->display_sel.si_note, DISPLAY_HINT_VBLANK(n, k));
		display_try_fence(o);	/* a commit waiting on its render */
		display_reap(o);
	}
}

/*
 * The IH HPD cause handler (sc->lock held): a monitor came or went on the
 * tallied pipes. Re-arm each for the next change, start a new generation for
 * the output and the topology (the next CONNECTOR or MODESET re-reads the
 * EDID) and wake NOTE_HOTPLUG listeners. The EDID is not read here: that is
 * up to four 128-byte DDC reads, too slow for an interrupt.
 */
static void
amd_display_hpd_handler(struct atrium_amd_softc *sc, int count __unused)
{
	struct atrium_amd_display_state *ds = sc->display_state;
	struct display_output *o;
	uint32_t n;

	for (n = 0; n < DISPLAY_NOUT; n++) {
		if (sc->ih_disp_count[ATRIUM_AMD_IH_CAUSE_HPD][n] == 0)
			continue;
		o = display_out(sc, n);
		amd_display_hw_ops.hpd_rearm(o);
		/*
		 * One counter for the whole display: an output's generation is
		 * the topology generation of its last change, so the
		 * NOTE_HOTPLUG data names the newest output generation.
		 */
		if (++ds->topo_gen == 0)
			ds->topo_gen = 1;
		o->conn_gen = ds->topo_gen;
		KNOTE_LOCKED(&sc->display_sel.si_note, DISPLAY_HINT_HOTPLUG);
	}
}

/* --- device-reset coordination: the display's prepare/restore hooks --------- */

/*
 * Prepare: a device-wide FLR is about to reset the display block (DCN), so
 * disarm the interrupts — no stale interrupt should fire mid-reset. Called by
 * the base reset coordinator (unlocked).
 */
static void
amd_display_reset_prepare(struct atrium_amd_softc *sc)
{
	struct display_output *o;
	uint32_t n;

	mtx_lock(&sc->lock);
	for (n = 0; n < DISPLAY_NOUT; n++) {
		o = display_out(sc, n);
		(void)display_frames(o);	/* bank them: the FLR restarts the OTG */
		display_irqs(o, 0, 0);
	}
	mtx_unlock(&sc->lock);
}

/*
 * Restore: the FLR is done. Re-apply the last committed state of every output
 * (GPU ABI v2.1 §10.2): the mode and the on-screen buffer, then the
 * interrupts. A flip that was in flight when the FLR hit was lost with the
 * hardware queue; count it as landed, since its buffer is the newest one the
 * compositor gave us. The buffers' contents were in VRAM the FLR may have
 * wiped; the compositor's next commits repaint them; NOTE_RESET and
 * STATUS_CONTENT_LOST tell it to.
 */
static void
amd_display_reset_restore(struct atrium_amd_softc *sc)
{
	struct display_output *o;
	uint32_t n;

	mtx_lock(&sc->lock);
	for (n = 0; n < DISPLAY_NOUT; n++) {
		o = display_out(sc, n);
		if (o->next_state == 2)
			display_landed(o);
		display_frames_rebase(o);
		o->content_lost = 1;
	}
	/*
	 * VRAM may not have survived the FLR (§10.3): whatever we re-apply below
	 * may be garbage until the holder commits a fresh frame. Say so in STATUS
	 * and wake the holder's NOTE_RESET (and vblank) knotes.
	 */
	KNOTE_LOCKED(&sc->display_sel.si_note, DISPLAY_HINT_RESET);
	for (n = 0; n < DISPLAY_NOUT; n++) {
		o = display_out(sc, n);
		amd_display_hw_ops.hpd_rearm(o);
		if (o->mode_set && o->on_screen.bo_fp != NULL)
			(void)display_hw_light(o, &o->on_screen,
			    o->timing_valid ? &o->timing : NULL, o->vrr_min_hz,
			    &o->hdr);
		else
			display_irqs(o, o->mode_set, 0);
	}
	display_sync_reapply(sc);
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
	uint32_t notes = kn->kn_sfflags &
	    (ATRIUM_DISPLAY_NOTE_RESET | ATRIUM_DISPLAY_NOTE_HOTPLUG);
	uint32_t outs = kn->kn_sfflags & ATRIUM_DISPLAY_NOTE_OUTPUTS;

	if (kn->kn_filter != EVFILT_READ ||
	    (kn->kn_sfflags & ~(ATRIUM_DISPLAY_NOTE_RESET |
	    ATRIUM_DISPLAY_NOTE_HOTPLUG | ATRIUM_DISPLAY_NOTE_OUTPUTS)) != 0 ||
	    (notes != 0 && outs != 0) ||
	    (outs & ~((ATRIUM_DISPLAY_NOTE_OUTPUT(DISPLAY_NOUT) - 1) &
	    ATRIUM_DISPLAY_NOTE_OUTPUTS)) != 0)
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
	struct display_output *o0 = display_out(sc, 0), *o;
	int err;

	switch (cmd) {
	case ATRIUM_DISPLAY_IOC_ENUM:
		display_enum(o0, (struct atrium_display_connector *)data);
		return (0);

	case ATRIUM_DISPLAY_IOC_MODES:
		display_modes(o0, (struct atrium_display_modes *)data);
		return (0);

	case ATRIUM_DISPLAY_IOC_CONNECTOR:
		return (display_connector(o0,
		    (struct atrium_display_connector_v2 *)data, NULL));

	case ATRIUM_DISPLAY_IOC_CONNECTOR_AT: {
		struct atrium_display_connector_at *c =
		    (struct atrium_display_connector_at *)data;

		if (c->struct_size < sizeof(*c) ||
		    (o = display_out(sc, c->output)) == NULL)
			return (EINVAL);
		return (display_connector(o, &c->conn, &c->out_identity));
	}

	case ATRIUM_DISPLAY_IOC_OUTPUTS:
		return (display_outputs(sc,
		    (struct atrium_display_outputs *)data));

	case ATRIUM_DISPLAY_IOC_COMMIT:
		return (display_commit(sc, (struct atrium_display_commit *)data,
		    td));

	case ATRIUM_DISPLAY_IOC_COMMIT_MULTI:
		return (display_commit_multi(sc,
		    (struct atrium_display_commit_multi *)data, td));

	case ATRIUM_DISPLAY_IOC_HDR:
		return (display_hdr(sc, (struct atrium_display_hdr *)data));

	case ATRIUM_DISPLAY_IOC_COMMIT_SYNC_GROUP:
		return (display_commit_sync_group(sc,
		    (struct atrium_display_sync_group *)data));

	case ATRIUM_DISPLAY_IOC_STATUS_V2:
		err = display_status(o0,
		    (struct atrium_display_status_v2 *)data);
		display_drain(sc, td);
		return (err);

	case ATRIUM_DISPLAY_IOC_STATUS_AT: {
		struct atrium_display_status_at *st =
		    (struct atrium_display_status_at *)data;

		if (st->struct_size < sizeof(*st) ||
		    (o = display_out(sc, st->output)) == NULL)
			return (EINVAL);
		err = display_status(o, &st->status);
		display_drain(sc, td);
		return (err);
	}

	case ATRIUM_DISPLAY_IOC_STATUS: {
		struct atrium_display_status *st =
		    (struct atrium_display_status *)data;

		mtx_lock(&sc->lock);
		st->vblank_count = display_frames(o0);
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
		display_sink_invalidate(o0);
		return (0);
	}

	case ATRIUM_DISPLAY_IOC_USBC: {
		struct atrium_display_usbc *u =
		    (struct atrium_display_usbc *)data;

		amd_mmio_write32(sc, regDISP_CFG_USBC, u->lanes);
		display_sink_invalidate(o0);
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
	struct display_output *o;
	uint32_t n;

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
	sc->display_state->topo_gen = 1;
	for (n = 0; n < DISPLAY_NOUT; n++) {
		o = display_out(sc, n);
		o->sc = sc;
		o->n = n;
		o->conn_gen = 1;
		TASK_INIT(&o->fence_task, 0, display_fence_task, o);
	}
	sc->display_cdev->si_drv1 = sc;

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
	    "simulator: plug monitor 0 VGA, 1 1080p, 2 4K, 3 desktop, "
	    "4 office, 5 desktop twin, 6 desktop (VRR from 60 Hz) into "
	    "output 0; -1 unplug");
	SYSCTL_ADD_PROC(device_get_sysctl_ctx(dev),
	    SYSCTL_CHILDREN(device_get_sysctl_tree(dev)), OID_AUTO,
	    "sim_plug_at", CTLTYPE_STRING | CTLFLAG_RW | CTLFLAG_MPSAFE, sc, 0,
	    display_sim_plug_at_sysctl, "A",
	    "simulator: OUTPUT:MODE, sim_plug for any output");
	SYSCTL_ADD_PROC(device_get_sysctl_ctx(dev),
	    SYSCTL_CHILDREN(device_get_sysctl_tree(dev)), OID_AUTO, "sim_sink",
	    CTLTYPE_STRING | CTLFLAG_RD | CTLFLAG_MPSAFE, sc, 0,
	    display_sim_sink_sysctl, "A",
	    "simulator: the monitor's view of the stream (HDR test readback)");
	SYSCTL_ADD_PROC(device_get_sysctl_ctx(dev),
	    SYSCTL_CHILDREN(device_get_sysctl_tree(dev)), OID_AUTO,
	    "sim_scanout", CTLTYPE_STRING | CTLFLAG_RD | CTLFLAG_MPSAFE, sc, 0,
	    display_sim_scanout_sysctl, "A",
	    "simulator: every pipe's scanout address, one snapshot (span test)");
	SYSCTL_ADD_PROC(device_get_sysctl_ctx(dev),
	    SYSCTL_CHILDREN(device_get_sysctl_tree(dev)), OID_AUTO,
	    "sim_bw_budget_khz", CTLTYPE_INT | CTLFLAG_RW | CTLFLAG_MPSAFE, sc,
	    0, display_sim_bw_sysctl, "I",
	    "simulator: the pixel rate the lit outputs share, kHz");

	/*
	 * Register our IH cause handlers so the base ISR routes DCN interrupts
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
	for (n = 0; n < DISPLAY_NOUT; n++)	/* hotplug interrupts from now on */
		amd_display_hw_ops.hpd_rearm(display_out(sc, n));
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

	device_printf(dev, "ready: /dev/atrium-display%d, %u outputs "
	    "(energy member %d)\n", device_get_unit(dev), DISPLAY_NOUT,
	    sc->display_energy_member);
	return (0);
}

static int
atrium_display_detach(device_t dev)
{
	struct atrium_amd_softc *sc = device_get_softc(device_get_parent(dev));
	struct display_output *o;
	uint32_t n;

	/*
	 * Stop the interrupts at the device, then clear our handlers under
	 * sc->lock (serializes against a firing ISR) so the base never routes into
	 * this module's text after it unloads.
	 */
	if (sc->display_energy_member >= 0) {
		energy_member_unregister(sc->display_energy_member);
		sc->display_energy_member = -1;
	}
	for (n = 0; n < DISPLAY_NOUT; n++) {
		o = display_out(sc, n);
		display_irqs(o, 0, 0);
		o_wr(o, HPD(o, regHPD0_DC_HPD_INT_CONTROL), DCN_HPD_INT_ACK);
	}
	amd_ih_set_handler(sc, ATRIUM_AMD_IH_CAUSE_VBLANK, NULL);
	amd_ih_set_handler(sc, ATRIUM_AMD_IH_CAUSE_FLIP, NULL);
	amd_ih_set_handler(sc, ATRIUM_AMD_IH_CAUSE_HPD, NULL);
	amd_reset_register(sc, ATRIUM_AMD_IP_DISPLAY, NULL, NULL);
	if (sc->display_cdev != NULL) {
		destroy_dev(sc->display_cdev);
		sc->display_cdev = NULL;
	}
	/*
	 * Let go of every buffer and fence the display still holds. The screens
	 * keep whatever the hardware last latched; nothing can flip them now.
	 */
	mtx_lock(&sc->lock);
	for (n = 0; n < DISPLAY_NOUT; n++) {
		o = display_out(sc, n);
		if (o->next_state != 0)
			display_abandon(o);	/* clears the in-fence watch */
		display_park(sc, o->on_screen.bo_fp);
		memset(&o->on_screen, 0, sizeof(o->on_screen));
		o->mode_set = 0;
		o->mode_width = 0;
		o->mode_height = 0;
	}
	sc->display_opened = 0;
	mtx_unlock(&sc->lock);
	for (n = 0; n < DISPLAY_NOUT; n++)
		taskqueue_drain(taskqueue_thread, &display_out(sc, n)->fence_task);
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
