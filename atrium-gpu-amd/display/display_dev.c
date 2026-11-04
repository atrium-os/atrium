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
#include <sys/energy_budget.h>		/* the display's energy-federation member */

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

static void
display_program_surface(struct atrium_amd_softc *sc,
    const struct atrium_amd_display_fb *fb)
{
	display_program_fb(sc, fb->vram_offset, fb->size);
	amd_mmio_write32(sc, regDISP_FB_PITCH, fb->pitch);
	amd_mmio_write32(sc, regDISP_FB_FORMAT, fb->format);
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

/* Has the flip we programmed landed? The model counts flips that reach the
 * screen; ours is the one that takes the count to display_next_flips. */
static void
display_reap(struct atrium_amd_softc *sc)
{
	uint32_t done;

	mtx_assert(&sc->lock, MA_OWNED);
	if (sc->display_next_state != 2)
		return;
	done = amd_mmio_read32(sc, regDISP_FLIP_DONE_COUNT);
	if ((int32_t)(done - (uint32_t)sc->display_next_flips) >= 0)
		display_landed(sc);
}

/* Program the outstanding commit's flip. Returns the referee fault (0 = ok);
 * on a fault nothing was queued. */
static uint32_t
display_flip_next(struct atrium_amd_softc *sc)
{
	uint32_t done, fault;

	mtx_assert(&sc->lock, MA_OWNED);
	done = amd_mmio_read32(sc, regDISP_FLIP_DONE_COUNT);
	display_program_surface(sc, &sc->display_next);
	amd_mmio_write32(sc, regDISP_FLIP, sc->display_next_vsync ? 1u : 0u);
	fault = amd_mmio_read32(sc, regDISP_FAULT);
	if (fault != 0)
		return (fault);
	sc->display_next_flips = (uint64_t)done + 1;
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
	ready = so->value >= sc->display_in_value;
	mtx_unlock(&so->lock);
	return (ready);
}

/*
 * A commit waiting on its in-fence is retried at every vblank (and when the
 * next commit comes in). A render that finishes mid-frame therefore flips at
 * the vblank after it, which is the frame it could have made anyway for a
 * vsync'd flip programmed then; an ASYNC flip may land up to a frame late.
 * Waking on the syncobj's own signal is the follow-up that removes that.
 */
static void
display_try_fence(struct atrium_amd_softc *sc)
{
	uint32_t fault;

	mtx_assert(&sc->lock, MA_OWNED);
	if (sc->display_next_state != 1 || !display_in_fence_ready(sc))
		return;
	fault = display_flip_next(sc);
	if (fault != 0) {
		sc->display_last_fault = fault;
		display_abandon(sc);
	}
}

/* IH FLIP cause: a flip reached the screen. sc->lock held. */
static void
amd_display_flip_handler(struct atrium_amd_softc *sc, int count __unused)
{
	display_reap(sc);
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
	struct atrium_display_modes modes;
	uint64_t off, size;
	uint32_t fault;
	int err;

	c->out_fault = 0;
	if (c->struct_size < sizeof(*c) || c->_pad != 0 ||
	    (c->flags & ~(ATRIUM_DISPLAY_COMMIT_MODESET |
	    ATRIUM_DISPLAY_COMMIT_ASYNC | ATRIUM_DISPLAY_COMMIT_TEST_ONLY)) != 0 ||
	    c->mode_index != 0 || c->format != ATRIUM_DISPLAY_FORMAT_XRGB8888 ||
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
	fb.vram_offset = off;
	fb.size = size;
	fb.pitch = c->pitch;
	fb.format = c->format;
	if (c->flags & ATRIUM_DISPLAY_COMMIT_MODESET)
		display_modes(sc, &modes);	/* reads the EDID; not under the lock */

	mtx_lock(&sc->lock);
	display_reap(sc);
	if (sc->display_next_state != 0) {
		mtx_unlock(&sc->lock);
		err = EBUSY;
		goto out;
	}

	if (c->flags & ATRIUM_DISPLAY_COMMIT_MODESET) {
		sc->display_in_so = in_so;
		if (!display_in_fence_ready(sc)) {
			sc->display_in_so = NULL;
			mtx_unlock(&sc->lock);
			err = EAGAIN;
			goto out;
		}
		sc->display_in_so = NULL;
		display_program_surface(sc, &fb);
		amd_mmio_write32(sc, regDISP_SET_MODE, 1);
		fault = amd_mmio_read32(sc, regDISP_FAULT);
		if (fault != 0) {
			mtx_unlock(&sc->lock);
			c->out_fault = fault;
			err = EINVAL;
			goto out;
		}
		/*
		 * The CRTC is live: arm the vblank and flip-done interrupts. The
		 * device raises them through the IH ring and the base ISR routes
		 * them here (interrupt-driven like real silicon).
		 */
		amd_mmio_write32(sc, regDISP_VBLANK_IRQ_EN, 1);
		amd_mmio_write32(sc, regDISP_FLIP_IRQ_EN, 1);
		sc->display_vblank_armed = 1;	/* remembered across a device reset */
		sc->display_mode_set = 1;
		sc->display_refresh_mhz = modes.count > 0 ?
		    modes.modes[0].refresh_mhz : 0;
		/* A modeset scans the new buffer out from its first frame. */
		display_park(sc, sc->display_on_screen.bo_fp);
		sc->display_on_screen = fb;
		if (out_so != NULL)
			ops->syncobj_signal(out_so, c->out_fence_value);
		display_park(sc, out_fp);
		display_park(sc, in_fp);
		mtx_unlock(&sc->lock);
		display_drain(sc, td);
		return (0);
	}

	if (!sc->display_mode_set) {
		mtx_unlock(&sc->lock);
		c->out_fault = ATRIUM_DISPLAY_FAULT_NO_MODE;
		err = EINVAL;
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
	if (display_in_fence_ready(sc)) {
		fault = display_flip_next(sc);
		if (fault != 0) {
			/* Refused now: report it to the caller, keep its references
			 * for it to release, and leave the screen as it was. */
			memset(&sc->display_next, 0, sizeof(sc->display_next));
			sc->display_next_state = 0;
			sc->display_in_fp = NULL;
			sc->display_in_so = NULL;
			sc->display_out_fp = NULL;
			sc->display_out_so = NULL;
			mtx_unlock(&sc->lock);
			c->out_fault = fault;
			err = EINVAL;
			goto out;
		}
	}
	mtx_unlock(&sc->lock);
	display_drain(sc, td);
	return (0);	/* the display owns the references now */

out:
	if (out_fp != NULL)
		fdrop(out_fp, td);
	if (in_fp != NULL)
		fdrop(in_fp, td);
	fdrop(bo_fp, td);
	return (err);
}

static int
display_open(struct cdev *cdev, int oflags, int devtype, struct thread *td)
{
	struct atrium_amd_softc *sc = cdev->si_drv1;
	int err = 0;

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
	/* Called under sc->lock (the knlist lock) by KNOTE_LOCKED / the kqueue
	 * scan. hint = vblanks this interrupt drained (0 on a bare readiness scan). */
	if (hint != 0)
		kn->kn_data += hint;
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
	KNOTE_LOCKED(&sc->display_sel.si_note, count);
	display_try_fence(sc);	/* a commit waiting on its render */
	display_reap(sc);
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
	amd_mmio_write32(sc, regDISP_VBLANK_IRQ_EN, 0);
	amd_mmio_write32(sc, regDISP_FLIP_IRQ_EN, 0);
}

/*
 * Restore: the FLR is done. Re-apply the last committed state (GPU ABI v2.1
 * §10.2): the mode and the on-screen buffer, then the interrupts. A flip that
 * was in flight when the FLR hit was lost with the hardware queue; count it as
 * landed, since its buffer is the newest one the compositor gave us. The
 * buffer's contents were in VRAM the FLR may have wiped; the compositor's next
 * commit repaints them. (The NOTE_RESET kqueue event is a follow-up.)
 */
static void
amd_display_reset_restore(struct atrium_amd_softc *sc)
{
	mtx_lock(&sc->lock);
	if (sc->display_next_state == 2)
		display_landed(sc);
	if (sc->display_mode_set && sc->display_on_screen.bo_fp != NULL) {
		display_program_surface(sc, &sc->display_on_screen);
		amd_mmio_write32(sc, regDISP_SET_MODE, 1);
		amd_mmio_write32(sc, regDISP_FLIP_IRQ_EN, 1);
	}
	if (sc->display_vblank_armed)
		amd_mmio_write32(sc, regDISP_VBLANK_IRQ_EN, 1);
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

	if (kn->kn_filter != EVFILT_READ)
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
		    (sc->display_mode_set ? ATRIUM_DISPLAY_STATUS_MODE_SET : 0);
		st->out_vblank_count = amd_mmio_read32(sc, regDISP_VBLANK_COUNT);
		st->out_flips_done = amd_mmio_read32(sc, regDISP_FLIP_DONE_COUNT);
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
	sc->display_cdev->si_drv1 = sc;

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
	amd_mmio_write32(sc, regDISP_VBLANK_IRQ_EN, 0);
	amd_mmio_write32(sc, regDISP_FLIP_IRQ_EN, 0);
	sc->display_vblank_armed = 0;
	amd_ih_set_handler(sc, ATRIUM_AMD_IH_CAUSE_VBLANK, NULL);
	amd_ih_set_handler(sc, ATRIUM_AMD_IH_CAUSE_FLIP, NULL);
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
		display_abandon(sc);
	display_park(sc, sc->display_on_screen.bo_fp);
	memset(&sc->display_on_screen, 0, sizeof(sc->display_on_screen));
	sc->display_mode_set = 0;
	sc->display_opened = 0;
	mtx_unlock(&sc->lock);
	display_drain(sc, curthread);
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
