/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * gfx12_pm.c — power P2 step 2 (docs/spec/gpu-power-management.md): the GC
 * 12.x register setup that lets graphics idle once the RLC is up, so the
 * PMFW does not run GC at full clock with nothing to do (the R9700 went to
 * 262 W within a second of RLC boot, 2026-11-12).
 *
 * Interface: what Linux's MIT-licensed gfx_v12_0.c does for GC 12.0.0/1 with
 * PSP firmware loading and the soc24.c cg_flags (CGCG, CGLS, MGCG, 3D CGCG/
 * CGLS, repeater FGCG, FGCG, perf clock):
 * - gfx_v12_0_rlc_resume, PSP branch: enable the RLC save/restore machine;
 * - gfx_v12_0_update_gfx_clock_gating(enable), inside RLC safe mode:
 *   coarse grain (overrides off, CGCG/CGLS FSM on, 3D likewise, CP idle
 *   poll count, CP/SDMA idle interrupts to the RLC), medium grain, repeater
 *   FGCG, SRAM FGCG, perf clock, GUI idle interrupt on ring 0.
 * Register offsets from gc_12_0_0_offset.h / gc_12_0_0_sh_mask.h. Picked by
 * GC's discovered version (12.0.x), never by device id.
 */
#include "atrium_gpu_amd.h"

/* Segment 0 */
#define GC12_CP_RB_WPTR_POLL_CNTL	0x0f62
#define GC12_GRBM_STATUS		0x0da4
#define GC12_CP_INT_CNTL		0x1de9
#define GC12_CP_INT_CNTL_RING0		0x1e0a
#define GC12_SDMA0_RLC_CGCG_CTRL	0x0055
#define GC12_SDMA1_RLC_CGCG_CTRL	0x0655
/* Segment 1 */
#define GC12_RLC_SAFE_MODE		0x0980
#define GC12_RLC_CNTL			0x4c00
#define GC12_RLC_STAT			0x4c04
#define GC12_RLC_CGTT_MGCG_OVERRIDE	0x4c48
#define GC12_RLC_CGCG_CGLS_CTRL		0x4c49
#define GC12_RLC_SRM_CNTL		0x4c80
#define GC12_RLC_CGCG_CGLS_CTRL_3D	0x4cc5

/* RLC_CGTT_MGCG_OVERRIDE */
#define OVR_RLC_REPEATER_FGCG	0x001
#define OVR_RLC_CGTT_SCLK	0x002
#define OVR_GFXIP_MGCG		0x004
#define OVR_GFXIP_CGCG		0x008
#define OVR_GFXIP_CGLS		0x010
#define OVR_GRBM_CGTT_SCLK	0x020
#define OVR_GFXIP_GFX3D_CG	0x080
#define OVR_GFXIP_FGCG		0x100
#define OVR_GFXIP_REPEATER_FGCG	0x200
#define OVR_PERFMON_CLOCK_STATE	0x400
/* RLC_CGCG_CGLS_CTRL(_3D) */
#define CGCG_EN			0x00000001
#define CGLS_EN			0x00000002
#define CGLS_DELAY_MASK		0x000000fc
#define CGLS_DELAY_SHIFT	2
#define CGCG_IDLE_THR_MASK	0x07ffff00
#define CGCG_IDLE_THR_SHIFT	8
/* CP_INT_CNTL(_RING0) idle interrupts */
#define CP_INT_IDLE_BITS	((1u << 18) | (1u << 19) | (1u << 20) | (1u << 21))
/* SDMA*_RLC_CGCG_CTRL */
#define SDMA_CGCG_INT_ENABLE	0x2
/* RLC_SRM_CNTL */
#define SRM_ENABLE		0x1
#define SRM_AUTO_INCR_ADDR	0x2
/* RLC_SAFE_MODE */
#define SAFE_MODE_CMD		0x1
#define SAFE_MODE_MSG_ENTER	(1u << 1)

static bool
gc_in(struct atrium_amd_softc *sc, u_int seg, uint32_t dw)
{
	return (sc->ip.gc.nbase > seg &&
	    ((bus_size_t)sc->ip.gc.base[seg] + dw) * 4 + 4 <=
	    rman_get_size(sc->regs));
}

static uint32_t
rd(struct atrium_amd_softc *sc, u_int seg, uint32_t dw)
{
	return (amd_mmio_read32(sc, ((bus_size_t)sc->ip.gc.base[seg] + dw) * 4));
}

static void
wr(struct atrium_amd_softc *sc, u_int seg, uint32_t dw, uint32_t v)
{
	amd_mmio_write32(sc, ((bus_size_t)sc->ip.gc.base[seg] + dw) * 4, v);
}

static void
rmw(struct atrium_amd_softc *sc, u_int seg, uint32_t dw, uint32_t clr,
    uint32_t set)
{
	uint32_t v = rd(sc, seg, dw), n = (v & ~clr) | set;

	if (n != v)
		wr(sc, seg, dw, n);
}

/* Every register this file touches is inside the GC aperture. */
static bool
gfx12_regs_ok(struct atrium_amd_softc *sc)
{
	static const struct { u_int seg; uint32_t dw; } regs[] = {
		{ 0, GC12_CP_RB_WPTR_POLL_CNTL }, { 0, GC12_GRBM_STATUS },
		{ 0, GC12_CP_INT_CNTL }, { 0, GC12_CP_INT_CNTL_RING0 },
		{ 0, GC12_SDMA0_RLC_CGCG_CTRL }, { 0, GC12_SDMA1_RLC_CGCG_CTRL },
		{ 1, GC12_RLC_SAFE_MODE }, { 1, GC12_RLC_CNTL },
		{ 1, GC12_RLC_STAT }, { 1, GC12_RLC_CGTT_MGCG_OVERRIDE },
		{ 1, GC12_RLC_CGCG_CGLS_CTRL }, { 1, GC12_RLC_SRM_CNTL },
		{ 1, GC12_RLC_CGCG_CGLS_CTRL_3D },
	};
	size_t i;

	if (!sc->ip.gc.present || sc->ip.gc.major != 12 ||
	    sc->ip.gc.minor != 0)
		return (false);
	for (i = 0; i < nitems(regs); i++)
		if (!gc_in(sc, regs[i].seg, regs[i].dw))
			return (false);
	return (true);
}

void
amd_gfx12_pm_report(struct atrium_amd_softc *sc, const char *when)
{
	if (!gfx12_regs_ok(sc))
		return;
	device_printf(sc->dev, "gfx: %s: GRBM_STATUS %#x, RLC_STAT %#x, "
	    "MGCG_OVERRIDE %#x, CGCG_CGLS %#x, CGCG_CGLS_3D %#x, SRM %#x, "
	    "CP_INT_CNTL_RING0 %#x\n", when, rd(sc, 0, GC12_GRBM_STATUS),
	    rd(sc, 1, GC12_RLC_STAT), rd(sc, 1, GC12_RLC_CGTT_MGCG_OVERRIDE),
	    rd(sc, 1, GC12_RLC_CGCG_CGLS_CTRL),
	    rd(sc, 1, GC12_RLC_CGCG_CGLS_CTRL_3D), rd(sc, 1, GC12_RLC_SRM_CNTL),
	    rd(sc, 0, GC12_CP_INT_CNTL_RING0));
}

/* gfx_v12_0_rlc_resume, PSP-loaded branch. */
int
amd_gfx12_srm_enable(struct atrium_amd_softc *sc)
{
	if (!gfx12_regs_ok(sc) || (rd(sc, 1, GC12_RLC_CNTL) & 1) == 0)
		return (ENXIO);
	rmw(sc, 1, GC12_RLC_SRM_CNTL, 0, SRM_ENABLE | SRM_AUTO_INCR_ADDR);
	return (0);
}

/* gfx_v12_0_set_safe_mode / unset_safe_mode. */
static int
safe_mode_enter(struct atrium_amd_softc *sc)
{
	int us;

	wr(sc, 1, GC12_RLC_SAFE_MODE, SAFE_MODE_CMD | SAFE_MODE_MSG_ENTER);
	for (us = 0; us < 100000; us++) {
		if ((rd(sc, 1, GC12_RLC_SAFE_MODE) & SAFE_MODE_CMD) == 0)
			return (0);
		DELAY(1);
	}
	device_printf(sc->dev, "gfx: RLC did not acknowledge safe mode\n");
	return (ETIMEDOUT);
}

static void
safe_mode_exit(struct atrium_amd_softc *sc)
{
	wr(sc, 1, GC12_RLC_SAFE_MODE, SAFE_MODE_CMD);
}

/*
 * gfx_v12_0_update_gfx_clock_gating for GC 12.0.0/1's cg_flags. `enable`
 * false undoes it (overrides back on, FSMs off), as amdgpu's ungate.
 */
int
amd_gfx12_clock_gating(struct atrium_amd_softc *sc, bool enable)
{
	int err;

	if (!gfx12_regs_ok(sc) || (rd(sc, 1, GC12_RLC_CNTL) & 1) == 0)
		return (ENXIO);
	err = safe_mode_enter(sc);
	if (err != 0)
		return (err);
	if (enable) {
		/* Coarse grain: overrides off, CGCG/CGLS FSMs on. */
		rmw(sc, 1, GC12_RLC_CGTT_MGCG_OVERRIDE,
		    OVR_GFXIP_CGCG | OVR_GFXIP_CGLS | OVR_GFXIP_GFX3D_CG, 0);
		rmw(sc, 1, GC12_RLC_CGCG_CGLS_CTRL,
		    CGCG_IDLE_THR_MASK | CGLS_DELAY_MASK,
		    (0x36u << CGCG_IDLE_THR_SHIFT) | CGCG_EN |
		    (0xfu << CGLS_DELAY_SHIFT) | CGLS_EN);
		rmw(sc, 1, GC12_RLC_CGCG_CGLS_CTRL_3D,
		    CGCG_IDLE_THR_MASK | CGLS_DELAY_MASK,
		    (0x36u << CGCG_IDLE_THR_SHIFT) | CGCG_EN |
		    (0xfu << CGLS_DELAY_SHIFT) | CGLS_EN);
		/* IDLE_POLL_COUNT 0x90, POLL_FREQUENCY 0x100. */
		wr(sc, 0, GC12_CP_RB_WPTR_POLL_CNTL, (0x0090u << 16) | 0x0100);
		rmw(sc, 0, GC12_CP_INT_CNTL, 0, CP_INT_IDLE_BITS);
		rmw(sc, 0, GC12_SDMA0_RLC_CGCG_CTRL, 0, SDMA_CGCG_INT_ENABLE);
		/* GC 12.0.x parts have two SDMA engines. */
		rmw(sc, 0, GC12_SDMA1_RLC_CGCG_CTRL, 0, SDMA_CGCG_INT_ENABLE);
		/* Medium grain, repeater FGCG, SRAM FGCG, perf clock. */
		rmw(sc, 1, GC12_RLC_CGTT_MGCG_OVERRIDE,
		    OVR_GRBM_CGTT_SCLK | OVR_RLC_CGTT_SCLK | OVR_GFXIP_MGCG |
		    OVR_GFXIP_REPEATER_FGCG | OVR_RLC_REPEATER_FGCG |
		    OVR_GFXIP_FGCG | OVR_PERFMON_CLOCK_STATE, 0);
		/* GUI idle interrupt, ring 0 (me 0 pipe 0). */
		rmw(sc, 0, GC12_CP_INT_CNTL_RING0, 0, CP_INT_IDLE_BITS);
	} else {
		rmw(sc, 1, GC12_RLC_CGCG_CGLS_CTRL, CGCG_EN | CGLS_EN, 0);
		rmw(sc, 1, GC12_RLC_CGCG_CGLS_CTRL_3D, CGCG_EN | CGLS_EN, 0);
		rmw(sc, 1, GC12_RLC_CGTT_MGCG_OVERRIDE, 0,
		    OVR_GRBM_CGTT_SCLK | OVR_RLC_CGTT_SCLK | OVR_GFXIP_MGCG |
		    OVR_GFXIP_REPEATER_FGCG | OVR_RLC_REPEATER_FGCG |
		    OVR_GFXIP_FGCG | OVR_PERFMON_CLOCK_STATE);
		rmw(sc, 0, GC12_CP_INT_CNTL_RING0, CP_INT_IDLE_BITS, 0);
	}
	safe_mode_exit(sc);
	return (0);
}

/* GC 12.0.x idle sequence (box-verified on 12.0.1, 2026-11-12). */
static int
gc12_idle(struct atrium_amd_softc *sc)
{
	int err;

	amd_gfx12_pm_report(sc, "before idle setup");
	err = amd_gfx12_srm_enable(sc);
	if (err == 0)
		err = amd_gfx12_clock_gating(sc, true);
	amd_gfx12_pm_report(sc, "after idle setup");
	return (err);
}

/*
 * What takes GC from powered to idle, per family (spec A3.9). The caller
 * then allows GfxOff and must see the card idle in telemetry. Another
 * family gets its own row (it may need power gating, a running CP or a
 * different handshake); one without a row gets ENOTSUP and is refused.
 */
static const struct {
	uint8_t	major, minor, rev_min, rev_max;
	const char *name;
	int	(*enter)(struct atrium_amd_softc *);
} gc_idle_gens[] = {
	{ 12, 0, 0, 1, "GC 12.0.0/1: SRM, clock gating", gc12_idle },
};

int
amd_gc_idle_enter(struct atrium_amd_softc *sc)
{
	size_t i;

	for (i = 0; i < nitems(gc_idle_gens); i++) {
		if (sc->ip.gc.present && sc->ip.gc.major == gc_idle_gens[i].major &&
		    sc->ip.gc.minor == gc_idle_gens[i].minor &&
		    sc->ip.gc.rev >= gc_idle_gens[i].rev_min &&
		    sc->ip.gc.rev <= gc_idle_gens[i].rev_max) {
			device_printf(sc->dev, "gfx: idle sequence %s\n",
			    gc_idle_gens[i].name);
			return (gc_idle_gens[i].enter(sc));
		}
	}
	device_printf(sc->dev, "gfx: no idle sequence for GC %u.%u.%u\n",
	    sc->ip.gc.major, sc->ip.gc.minor, sc->ip.gc.rev);
	return (ENOTSUP);
}
