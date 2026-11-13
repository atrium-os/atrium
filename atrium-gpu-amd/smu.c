/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * smu.c — stage 3d of the silicon bring-up (docs/spec/r9700-bare-metal.md
 * §4.3): bring the SMU's power-management firmware (PMFW) to the state
 * amdgpu's smu_hw_init leaves it in before GFX waits for RLC autoload.
 * Opt-in and test-only: runs from the base attach on silicon only with the
 * kenv hw.atrium_gpu.smu_init=1, and the attach still refuses afterwards.
 *
 * Interface: the MP1 message mailbox as Linux's MIT-licensed smu_cmn.c and
 * smu_v14_0.c / smu_v14_0_2_ppt.c drive it (offsets mp_14_0_2_offset.h,
 * message ids smu_v14_0_2_ppsmc.h). Selected by MP1's discovered version
 * (14.x). Protocol: wait for the previous response (C2PMSG_90 != 0), clear
 * it, write the argument (C2PMSG_82), write the message id (C2PMSG_66),
 * poll C2PMSG_90: 1 = OK, 0xFE unknown, 0xFD bad prerequisite, 0xFF failed.
 * Output arguments come back in C2PMSG_82.
 *
 * Rules from docs/spec/gpu-power-management.md: only the allowlisted
 * messages below can be sent; protection features must be running; a bad
 * answer or timeout stops all SMU traffic; the PMFW's own idle policy
 * (GfxOff allowed) is handed back at the end.
 *
 * Vendor-neutral split (spec §2, §8): everything SMU-generation-specific
 * (mailbox registers, message ids, required feature bits, table ids) is a
 * row of smu_gens[], picked by MP1's discovered version; the metrics
 * layout is picked by the PMFW's driver interface version in Rust
 * (rs/smu_metrics.rs) and comes out as the neutral telemetry record
 * (rs/power.rs). An MP1 version with no row gets no messages at all; a
 * driver interface with no layout gets no decoded telemetry.
 *
 * Sequence (smu_hw_init / smu_smc_hw_setup for 14.0.2/3, no SCPM writes):
 * TestMessage, GetSmuVersion, GetDriverIfVersion, SetDriverDramAddr (a VRAM
 * buffer for later table transfers), RunDcBtc, SetAllowedFeaturesMask (all,
 * as smu_v14_0_2_init_allowed_features; refused under SCPM, then skipped as
 * amdgpu does), EnableAllSmuFeatures; then, for
 * bring-up, DisallowGfxOff so GC registers stay readable, report the RLC,
 * read the metrics table twice, and AllowGfxOff again.
 */
#include "atrium_gpu_amd.h"

#include <sys/sx.h>
#include <sys/taskqueue.h>
#include <dev/pci/pcireg.h>
#include <dev/pci/pcivar.h>

#define SMU_RESP_OK		0x01
#define SMU_RESP_BAD_PREREQ	0xfd
#define SMU_WAIT_US		2000000

/*
 * The allowlist (docs/spec/gpu-power-management.md §2 rule 1, §3): the only
 * messages this driver can express. smu_send() takes one of these, never a
 * raw id, and looks its id up in the SMU generation's row; adding a message
 * is a spec change.
 */
enum amd_smu_msg {
	SMU_TEST,
	SMU_GET_VERSION,
	SMU_GET_DRIVER_IF,
	SMU_SET_DRIVER_ADDR_HI,
	SMU_SET_DRIVER_ADDR_LO,
	SMU_RUN_DC_BTC,
	SMU_SET_ALLOWED_HI,
	SMU_SET_ALLOWED_LO,
	SMU_ENABLE_ALL,
	SMU_GET_RUNNING_LO,
	SMU_GET_RUNNING_HI,
	SMU_TABLE_SMU2DRAM,
	SMU_ALLOW_GFXOFF,
	SMU_DISALLOW_GFXOFF,
	SMU_NMSG
};
static const char *const smu_msg_names[SMU_NMSG] = {
	[SMU_TEST] = "TestMessage",
	[SMU_GET_VERSION] = "GetSmuVersion",
	[SMU_GET_DRIVER_IF] = "GetDriverIfVersion",
	[SMU_SET_DRIVER_ADDR_HI] = "SetDriverDramAddrHigh",
	[SMU_SET_DRIVER_ADDR_LO] = "SetDriverDramAddrLow",
	[SMU_RUN_DC_BTC] = "RunDcBtc",
	[SMU_SET_ALLOWED_HI] = "SetAllowedFeaturesMaskHigh",
	[SMU_SET_ALLOWED_LO] = "SetAllowedFeaturesMaskLow",
	[SMU_ENABLE_ALL] = "EnableAllSmuFeatures",
	[SMU_GET_RUNNING_LO] = "GetRunningSmuFeaturesLow",
	[SMU_GET_RUNNING_HI] = "GetRunningSmuFeaturesHigh",
	[SMU_TABLE_SMU2DRAM] = "TransferTableSmu2Dram",
	[SMU_ALLOW_GFXOFF] = "AllowGfxOff",
	[SMU_DISALLOW_GFXOFF] = "DisallowGfxOff",
};

/*
 * One SMU generation: what differs between MP1 versions. A message id of 0
 * means the generation has no such message, and smu_send() refuses it.
 */
struct smu_gen {
	uint8_t		mp1_major, mp1_minor, mp1_rev_min, mp1_rev_max;
	const char	*name;
	/* Mailbox, dword offsets in MP1 segment 1. */
	uint32_t	reg_msg, reg_arg, reg_resp;
	uint16_t	msg[SMU_NMSG];
	/*
	 * Debug mailbox (no response handshake), used for one thing: the
	 * ASIC reset that puts the card back in its post-POST state on
	 * detach (spec A3.6). 0 = the generation has none.
	 */
	uint32_t	reg_dbg_param, reg_dbg_msg, reg_dbg_resp;
	uint16_t	dbg_mode1_reset;
	/* Features that must be running before the GPU is used (spec §4). */
	uint64_t	required;
	uint32_t	table_metrics;
};

static const struct smu_gen smu_gens[] = {
	{
		/*
		 * SMU 14.0.2 / 14.0.3 (RDNA4): smu_v14_0_2_ppsmc.h ids,
		 * mp_14_0_2_offset.h C2PMSG_66/82/90, required features from
		 * smu14_driver_if_v14_0.h: FW_DSTATE 17, THROTTLERS 23,
		 * VR0HOT 27, FW_CTF 28, FAN_CONTROL 29, MEM_TEMP_READ 46.
		 */
		.mp1_major = 14, .mp1_minor = 0, .mp1_rev_min = 2,
		.mp1_rev_max = 3, .name = "SMU 14.0.2/3",
		.reg_msg = 0x82, .reg_arg = 0x92, .reg_resp = 0x9a,
		/* C2PMSG_53/75/54; DEBUGSMC_MSG_Mode1Reset (smu_v14_0_2_ppt.c). */
		.reg_dbg_param = 0x75, .reg_dbg_msg = 0x8b, .reg_dbg_resp = 0x76,
		.dbg_mode1_reset = 2,
		.msg = {
			[SMU_TEST] = 0x01,
			[SMU_GET_VERSION] = 0x02,
			[SMU_GET_DRIVER_IF] = 0x03,
			[SMU_SET_DRIVER_ADDR_HI] = 0x0e,
			[SMU_SET_DRIVER_ADDR_LO] = 0x0f,
			[SMU_RUN_DC_BTC] = 0x36,
			[SMU_SET_ALLOWED_HI] = 0x05,
			[SMU_SET_ALLOWED_LO] = 0x04,
			[SMU_ENABLE_ALL] = 0x06,
			[SMU_GET_RUNNING_LO] = 0x0c,
			[SMU_GET_RUNNING_HI] = 0x0d,
			[SMU_TABLE_SMU2DRAM] = 0x12,
			[SMU_ALLOW_GFXOFF] = 0x28,
			[SMU_DISALLOW_GFXOFF] = 0x29,
		},
		.required = (1ull << 17) | (1ull << 23) | (1ull << 27) |
		    (1ull << 28) | (1ull << 29) | (1ull << 46),
		.table_metrics = 5,
	},
};

static const struct smu_gen *
smu_gen_find(const struct atrium_amd_softc *sc)
{
	size_t i;

	if (!sc->ip.mp1.present)
		return (NULL);
	for (i = 0; i < nitems(smu_gens); i++) {
		const struct smu_gen *g = &smu_gens[i];

		if (sc->ip.mp1.major == g->mp1_major &&
		    sc->ip.mp1.minor == g->mp1_minor &&
		    sc->ip.mp1.rev >= g->mp1_rev_min &&
		    sc->ip.mp1.rev <= g->mp1_rev_max)
			return (g);
	}
	return (NULL);
}

CTASSERT(sizeof(struct atrium_gpu_power_telemetry) == 36);
size_t	atrium_amd_smu_metrics_len(uint32_t ifver);		/* Rust */
int	atrium_amd_smu_metrics(const uint8_t *b, size_t len, uint32_t ifver,
	    struct atrium_gpu_power_telemetry *out);		/* Rust */
#define SMU_METRICS_MAX		512

/* One sender, one state: after a bad answer nothing more is sent (rule 4). */
struct smu_link {
	struct atrium_amd_softc *sc;
	const struct smu_gen *gen;
	bool failed;
};

/* MMHUB 4.x FB location (as psp.c), and the SMU driver table in VRAM. */
#define MMHUB4_FB_LOCATION_BASE	0x554
#define MMHUB4_FB_BASE_MASK	0x00ffffffu
#define SMU_TABLE_VRAM_OFF	(84u << 20)
#define SMU_TABLE_SIZE		(1u << 20)

/* GC 12.x (gc_12_0_0_offset.h). */
#define GC12_GB_ADDR_CONFIG		0x13de	/* seg 0 */
#define GC12_CP_STAT			0x0f40	/* seg 0 */
#define GC12_RLC_RLCS_BOOTLOAD_STATUS	0x4e7c	/* seg 1 */
#define GC12_BOOTLOAD_COMPLETE		0x80000000u
#define GC_WAIT_US			2000000

static bus_size_t
smu_reg(struct atrium_amd_softc *sc, uint32_t dw)
{
	return (((bus_size_t)sc->ip.mp1.base[1] + dw) * 4);
}

static uint32_t
smu_poll(struct smu_link *l)
{
	struct atrium_amd_softc *sc = l->sc;
	uint32_t v = 0;
	int us;

	for (us = 0; us < SMU_WAIT_US; us += 10) {
		v = amd_mmio_read32(sc, smu_reg(sc, l->gen->reg_resp));
		if (v != 0)
			break;
		DELAY(10);
	}
	return (v);
}

/*
 * Send allowlisted message `m`. Returns the raw response; *out gets
 * C2PMSG_82 after an OK. `bad_prereq_ok`: 0xFD is an expected answer here
 * (SetAllowedFeaturesMask under SCPM) and does not fail the link. Any other
 * non-OK answer or a timeout marks the link failed: later calls send nothing
 * and return 0, and the PMFW is left in its own control.
 */
static uint32_t
smu_send(struct smu_link *l, enum amd_smu_msg m, uint32_t arg, uint32_t *out,
    bool bad_prereq_ok)
{
	struct atrium_amd_softc *sc = l->sc;
	uint32_t resp;

	if (l->failed || m >= SMU_NMSG)
		return (0);
	if (l->gen->msg[m] == 0) {
		device_printf(sc->dev, "smu: %s has no %s; not sent\n",
		    l->gen->name, smu_msg_names[m]);
		return (0);
	}
	resp = smu_poll(l);
	if (resp == 0) {
		device_printf(sc->dev, "smu: %s: previous message never "
		    "answered; link failed, PMFW left alone\n", smu_msg_names[m]);
		l->failed = true;
		return (0);
	}
	amd_mmio_write32(sc, smu_reg(sc, l->gen->reg_resp), 0);
	amd_mmio_write32(sc, smu_reg(sc, l->gen->reg_arg), arg);
	amd_mmio_write32(sc, smu_reg(sc, l->gen->reg_msg), l->gen->msg[m]);
	resp = smu_poll(l);
	if (resp == SMU_RESP_OK && out != NULL)
		*out = amd_mmio_read32(sc, smu_reg(sc, l->gen->reg_arg));
	device_printf(sc->dev, "smu: %-26s arg %#010x: resp %#x%s",
	    smu_msg_names[m], arg, resp, resp == 0 ? " (timeout)" : "");
	if (resp == SMU_RESP_OK && out != NULL)
		printf(", out %#x", *out);
	printf("\n");
	if (resp != SMU_RESP_OK &&
	    !(resp == SMU_RESP_BAD_PREREQ && bad_prereq_ok)) {
		device_printf(sc->dev, "smu: link failed; no further messages, "
		    "PMFW left in its own control\n");
		l->failed = true;
	}
	return (resp);
}

static bool
gc_ok(struct atrium_amd_softc *sc, u_int seg, uint32_t dw)
{
	return (sc->ip.gc.nbase > seg &&
	    ((bus_size_t)sc->ip.gc.base[seg] + dw) * 4 + 4 <=
	    rman_get_size(sc->regs));
}

static uint32_t
gc_rd(struct atrium_amd_softc *sc, u_int seg, uint32_t dw)
{
	return (amd_mmio_read32(sc, ((bus_size_t)sc->ip.gc.base[seg] + dw) * 4));
}

static void
gc_report(struct atrium_amd_softc *sc, const char *when)
{
	device_printf(sc->dev, "smu: %s: GB_ADDR_CONFIG %#x, RLC bootload "
	    "status %#x, CP_STAT %#x\n", when,
	    gc_rd(sc, 0, GC12_GB_ADDR_CONFIG),
	    gc_rd(sc, 1, GC12_RLC_RLCS_BOOTLOAD_STATUS),
	    gc_rd(sc, 0, GC12_CP_STAT));
}

static uint64_t
smu_features(struct smu_link *l)
{
	uint32_t lo = 0, hi = 0;

	(void)smu_send(l, SMU_GET_RUNNING_LO, 0, &lo, false);
	(void)smu_send(l, SMU_GET_RUNNING_HI, 0, &hi, false);
	device_printf(l->sc->dev, "smu: running features %#010x%08x\n", hi, lo);
	return ((uint64_t)hi << 32 | lo);
}

/*
 * TransferTableSmu2Dram(SMU_METRICS) into the driver table, read it through
 * BAR0, hand it to the Rust decoder (layout by driver interface version,
 * range-checked; untrusted bytes), which returns the neutral telemetry
 * record. Returns true if `t` holds a sample.
 */
static bool
smu_metrics(struct smu_link *l, struct resource *vram, uint32_t ifver,
    bool dump, struct atrium_gpu_power_telemetry *t)
{
	struct atrium_amd_softc *sc = l->sc;
	uint8_t b[SMU_METRICS_MAX];
	size_t len = atrium_amd_smu_metrics_len(ifver);
	int r, i;

	if (len == 0 || len > sizeof(b)) {
		device_printf(sc->dev, "smu: telemetry unavailable: no metrics "
		    "layout for driver interface %#x\n", ifver);
		return (false);
	}
	if (smu_send(l, SMU_TABLE_SMU2DRAM, l->gen->table_metrics, NULL,
	    false) != SMU_RESP_OK)
		return (false);
	bus_read_region_1(vram, SMU_TABLE_VRAM_OFF, b, len);
	if (dump) {
		/* Raw once, to check the layout by eye. */
		for (i = 0; i < (int)len; i += 32)
			device_printf(sc->dev, "smu: metrics +%3d: %*D\n", i,
			    (int)MIN(32, len - i), b + i, " ");
	}
	r = atrium_amd_smu_metrics(b, len, ifver, t);
	if (r < 0) {
		device_printf(sc->dev, "smu: telemetry unavailable (%s %d; "
		    "driver interface %#x)\n", r <= -10 ?
		    "implausible field" : "decode error", r <= -10 ? -r - 10 :
		    -r, ifver);
		return (false);
	}
	device_printf(sc->dev, "smu: telemetry #%u: edge %u C, hotspot %u C, "
	    "mem %u C, VR %u C; power %u W of %u W limit, board %u W; fan "
	    "%u rpm (%u%%); gfx %u MHz, mem %u MHz, gfx busy %u%%; throttle "
	    "%#x (ids %#x)\n", t->sample, t->edge_c, t->hotspot_c, t->mem_c,
	    t->vr_c, t->power_w, t->power_limit_w, t->board_power_w,
	    t->fan_rpm, t->fan_pct, t->gfx_mhz, t->mem_mhz, t->gfx_busy_pct,
	    t->throttle, t->vendor_throttle);
	return (true);
}

/*
 * Up to `n` telemetry samples 1 s apart, labelled. Returns true as soon as
 * one shows the card idle (spec A3.9: idle is proven, not assumed).
 */
static bool
smu_sample(struct smu_link *l, struct resource *vram, uint32_t ifver, int n,
    const char *label)
{
	struct atrium_gpu_power_telemetry t;
	int i;

	for (i = 0; i < n && !l->failed; i++) {
		if (i > 0)
			pause("smumet", hz);
		device_printf(l->sc->dev, "smu: sample [%s] %d/%d\n", label,
		    i + 1, n);
		if (!smu_metrics(l, vram, ifver, false, &t))
			continue;
		if (!atrium_gpu_power_idle_suspicious(&t))
			return (true);
		device_printf(l->sc->dev, "smu: busy or at the power limit "
		    "with no work queued\n");
	}
	return (false);
}

/*
 * The checks every SMU user makes (generation table, register windows, an
 * MMHUB 4 and GC 12 to talk about), then BAR0 for the driver table. On 0,
 * *vramp is the BAR (release with rid *ridp) and *mcp the table's MC address.
 */
static int
smu_open(struct atrium_amd_softc *sc, struct smu_link *l,
    struct resource **vramp, int *ridp, uint64_t *mcp)
{
	bus_size_t lim = rman_get_size(sc->regs);
	struct resource *vram;
	uint64_t fb_base;
	int rid;

	*vramp = NULL;
	l->sc = sc;
	l->failed = false;
	l->gen = smu_gen_find(sc);
	if (l->gen == NULL || sc->ip.mp1.nbase < 2 ||
	    smu_reg(sc, l->gen->reg_msg) + 4 > lim ||
	    smu_reg(sc, l->gen->reg_arg) + 4 > lim ||
	    smu_reg(sc, l->gen->reg_resp) + 4 > lim) {
		device_printf(sc->dev, "smu: SMU %u.%u.%u: no generation "
		    "table for it; not touching it\n", sc->ip.mp1.major,
		    sc->ip.mp1.minor, sc->ip.mp1.rev);
		return (ENOTSUP);
	}
	if (!sc->ip.mmhub.present || sc->ip.mmhub.major != 4 ||
	    ((bus_size_t)sc->ip.mmhub.base[0] + MMHUB4_FB_LOCATION_BASE) * 4 +
	    4 > lim)
		return (ENOTSUP);
	if (!sc->ip.gc.present || sc->ip.gc.major != 12 ||
	    !gc_ok(sc, 0, GC12_GB_ADDR_CONFIG) ||
	    !gc_ok(sc, 1, GC12_RLC_RLCS_BOOTLOAD_STATUS))
		return (ENOTSUP);
	rid = PCIR_BAR(0);
	vram = bus_alloc_resource_any(sc->dev, SYS_RES_MEMORY, &rid, RF_ACTIVE);
	if (vram == NULL ||
	    rman_get_size(vram) < SMU_TABLE_VRAM_OFF + SMU_TABLE_SIZE) {
		device_printf(sc->dev, "smu: BAR0 (VRAM window) unusable\n");
		if (vram != NULL)
			bus_release_resource(sc->dev, SYS_RES_MEMORY, rid, vram);
		return (ENXIO);
	}
	fb_base = (uint64_t)(amd_mmio_read32(sc,
	    ((bus_size_t)sc->ip.mmhub.base[0] + MMHUB4_FB_LOCATION_BASE) * 4) &
	    MMHUB4_FB_BASE_MASK) << 24;
	*mcp = fb_base + SMU_TABLE_VRAM_OFF;
	*vramp = vram;
	*ridp = rid;
	return (0);
}

/*
 * Start the SMU (spec §4.3) and prove the card idle (A3.9): test messages,
 * the driver table, the features (GC powers up; needs gc_up_ok), the
 * required protections, then the GC idle sequence, AllowGfxOff and an idle
 * telemetry sample within 5 s. Leaves GfxOff allowed. *ifverp gets the
 * driver interface version for metrics. 0 only if the card was proven idle.
 */
static int
smu_start(struct smu_link *lp, struct resource *vram, uint64_t mc,
    uint32_t *ifverp)
{
	struct atrium_amd_softc *sc = lp->sc;
	struct atrium_gpu_power_telemetry t;
	uint64_t feat;
	uint32_t v, ifver = 0;
	int us, r, err = 0, gc_up_ok = 0, gfx_cg;

#define	SEND(m, a, o)	smu_send(lp, (m), (a), (o), false)
	device_printf(sc->dev, "smu: SMU %u.%u.%u, response register %#x\n",
	    sc->ip.mp1.major, sc->ip.mp1.minor, sc->ip.mp1.rev,
	    amd_mmio_read32(sc, smu_reg(sc, lp->gen->reg_resp)));
	gc_report(sc, "before");

	if (SEND(SMU_TEST, 0, &v) != SMU_RESP_OK ||
	    SEND(SMU_GET_VERSION, 0, &v) != SMU_RESP_OK ||
	    SEND(SMU_GET_DRIVER_IF, 0, &ifver) != SMU_RESP_OK) {
		err = EIO;
		return (err);
	}
	bus_set_region_4(vram, SMU_TABLE_VRAM_OFF, 0, SMU_TABLE_SIZE / 4);
	if (SEND(SMU_SET_DRIVER_ADDR_HI, (uint32_t)(mc >> 32), NULL) !=
	    SMU_RESP_OK ||
	    SEND(SMU_SET_DRIVER_ADDR_LO, (uint32_t)mc, NULL) != SMU_RESP_OK) {
		err = EIO;
		return (err);
	}
	feat = smu_features(lp);
	if (lp->failed) {
		err = EIO;
		return (err);
	}
	/*
	 * Enabling the features powers GC up. Until the driver can bring GC
	 * back to idle itself (power P2), that leaves the card at full clock
	 * and its power limit with nothing running: the R9700 on 2026-11-12
	 * sat at 299 W, hotspot 98 C, until it was powered off. So it takes
	 * a second, explicit opt-in.
	 */
	TUNABLE_INT_FETCH("hw.atrium_gpu.gc_up_ok", &gc_up_ok);
	if (feat == 0 && !gc_up_ok) {
		device_printf(sc->dev, "smu: features not running; enabling "
		    "them powers GC up and the driver cannot idle it yet "
		    "(power P2). Not enabling (hw.atrium_gpu.gc_up_ok=0)\n");
		return (ECANCELED);	/* GC not up: nothing to attach to */
	}
	if (feat == 0) {
		/* First start this boot: amdgpu's smc_hw_setup order. */
		if (SEND(SMU_RUN_DC_BTC, 0, NULL) != SMU_RESP_OK) {
			err = EIO;
			return (err);
		}
		/*
		 * Under SCPM the PSP owns the feature mask and the PMFW
		 * refuses the driver's with 0xFD; amdgpu skips it then.
		 */
		v = smu_send(lp, SMU_SET_ALLOWED_HI, 0xffffffff, NULL, true);
		if (v == SMU_RESP_OK)
			v = smu_send(lp, SMU_SET_ALLOWED_LO, 0xffffffff,
			    NULL, true);
		if (v == SMU_RESP_BAD_PREREQ)
			device_printf(sc->dev, "smu: allowed mask refused: "
			    "SCPM, the PSP owns it\n");
		if (lp->failed ||
		    SEND(SMU_ENABLE_ALL, 0, NULL) != SMU_RESP_OK) {
			err = EIO;
			return (err);
		}
		feat = smu_features(lp);
	} else
		device_printf(sc->dev, "smu: features already running this "
		    "boot; not enabling again\n");
	if ((feat & lp->gen->required) != lp->gen->required) {
		device_printf(sc->dev, "smu: required protection features "
		    "missing (%#jx of %#jx); refusing the GPU\n",
		    (uintmax_t)(feat & lp->gen->required),
		    (uintmax_t)lp->gen->required);
		err = EIO;
		return (err);
	}
	device_printf(sc->dev, "smu: protection features running (thermal "
	    "trip, throttlers, fan, VR hot, memory temperature)\n");

	/* Bring-up only: keep GC readable while we look at it. */
	(void)SEND(SMU_DISALLOW_GFXOFF, 0, NULL);
	for (us = 0; us < GC_WAIT_US; us += 100) {
		if ((gc_rd(sc, 1, GC12_RLC_RLCS_BOOTLOAD_STATUS) &
		    GC12_BOOTLOAD_COMPLETE) && gc_rd(sc, 0, GC12_CP_STAT) == 0)
			break;
		DELAY(100);
	}
	device_printf(sc->dev, "smu: RLC autoload %s after %d us\n",
	    us < GC_WAIT_US ? "complete" : "NOT complete", us);
	gc_report(sc, "after");

	/*
	 * Telemetry, read only (spec §4). Nothing is queued here, so a busy
	 * or limit-bound card is reported (§A5 idle check).
	 */
	(void)smu_metrics(lp, vram, ifver, true, &t);
	pause("smumet", hz);
	(void)smu_sample(lp, vram, ifver, 1, "GC up, no idle setup");

	/*
	 * Spec A3.9: GC was powered up, so the family's idle sequence runs
	 * (gc_idle_gens[] in gfx12_pm.c, by GC version), GfxOff is allowed,
	 * and telemetry must show the card idle within 5 s. A family with
	 * no sequence, or a card that does not go idle, is refused; detach
	 * then resets it to post-POST. hw.atrium_gpu.gfx_cg=0 skips the
	 * sequence (diagnosis only; the check still runs and refuses).
	 */
	gfx_cg = 1;
	TUNABLE_INT_FETCH("hw.atrium_gpu.gfx_cg", &gfx_cg);
	if (!lp->failed) {
		r = gfx_cg ? amd_gc_idle_enter(sc) : ECANCELED;

		if (r != 0)
			device_printf(sc->dev, "gfx: no idle sequence run "
			    "(%d)\n", r);
		(void)SEND(SMU_ALLOW_GFXOFF, 0, NULL);
		if (smu_sample(lp, vram, ifver, 5, "idle check"))
			device_printf(sc->dev, "smu: card proven idle\n");
		else {
			device_printf(sc->dev, "smu: could not prove the card "
			    "idle within 5 s (spec A3.9); refusing it\n");
			err = EIO;
		}
		amd_gfx12_pm_report(sc, "end");
	}

	*ifverp = ifver;
	/*
	 * Not GB_ADDR_CONFIG here: with GfxOff allowed GC is off and reads 0
	 * (box run 44649060 refused a healthy card that way).
	 */
	if (err == 0 && lp->failed)
		err = EIO;
	return (err);
#undef SEND
}

int
amd_smu_init(struct atrium_amd_softc *sc)
{
	struct resource *vram = NULL;
	uint64_t mc;
	struct smu_link link;
	uint32_t ifver = 0;
	int rid, us, r, err = 0, gmc = 0, ih = 0, gfx = 0, gfx_r = -1, mes = 0;

#define	SEND(m, a, o)	smu_send(&link, (m), (a), (o), false)
	err = smu_open(sc, &link, &vram, &rid, &mc);
	if (err != 0)
		return (err);
	err = smu_start(&link, vram, mc, &ifver);
	if (err != 0)
		goto out;

	/*
	 * Stage 4a (kenv hw.atrium_gpu.gmc=1): hubs + GART, with GC held
	 * powered for the GFXHUB writes (scoped: always re-allowed), then
	 * the idle check again. Undone before the detach reset.
	 */
	TUNABLE_INT_FETCH("hw.atrium_gpu.gmc", &gmc);
	if (gmc && err == 0 && !link.failed) {
		struct amd_gart gt;

		(void)SEND(SMU_DISALLOW_GFXOFF, 0, NULL);
		for (us = 0; us < GC_WAIT_US && gc_rd(sc, 0,
		    GC12_GB_ADDR_CONFIG) == 0; us += 100)
			DELAY(100);
		r = amd_gmc12_gart_enable(sc, vram, &gt);
		device_printf(sc->dev, "gmc: stage 4a: %d\n", r);
		/* Stage 4c (kenv hw.atrium_gpu.gfx=1): GFX setup, CP halted. */
		TUNABLE_INT_FETCH("hw.atrium_gpu.gfx", &gfx);
		if (gfx && r == 0) {
			gfx_r = amd_gfx12_setup(sc);
			device_printf(sc->dev, "gfx12: stage 4c: %d\n", gfx_r);
		}
		/*
		 * Stage 4d (kenv hw.atrium_gpu.mes=1, needs 4c): MES up and
		 * answering, run inside the IH test (4b) when that is on so
		 * MES's fence interrupts prove the IH write path.
		 */
		TUNABLE_INT_FETCH("hw.atrium_gpu.mes", &mes);
		if (mes && !(gfx && gfx_r == 0)) {
			device_printf(sc->dev, "mes: stage 4d needs stage 4c "
			    "(hw.atrium_gpu.gfx=1) to pass; skipped\n");
			mes = 0;
		}
		/* Stage 4b (kenv hw.atrium_gpu.ih=1): IH ring 0 + MSI. */
		TUNABLE_INT_FETCH("hw.atrium_gpu.ih", &ih);
		if (ih && r == 0)
			(void)amd_ih7_test(sc, &gt, mes ? amd_mes12_test : NULL);
		else if (mes && r == 0)
			(void)amd_mes12_test(sc, &gt);
		amd_gmc12_gart_disable(&gt);
		(void)SEND(SMU_ALLOW_GFXOFF, 0, NULL);
		if (smu_sample(&link, vram, ifver, 5, "idle after stage 4"))
			device_printf(sc->dev, "smu: card proven idle after "
			    "stage 4\n");
		else {
			device_printf(sc->dev, "smu: not idle after stage "
			    "4; refusing\n");
			err = EIO;
		}
	}

	/* Hand back the PMFW's own idle policy (rule 6). */
	(void)SEND(SMU_ALLOW_GFXOFF, 0, NULL);
	if (link.failed && err == 0)
		err = EIO;
#undef SEND
out:
	if (vram != NULL)
		bus_release_resource(sc->dev, SYS_RES_MEMORY, rid, vram);
	return (err);
}

/*
 * Mode 1 reset (spec A3.6, B1): the whole ASIC but the PCIe link goes back
 * to its post-POST state: PSP bootloader waiting, no sOS, no IP firmware,
 * GC off, PMFW from the board's own ROM. That is the state the card idled
 * in before the driver touched it, so it is how the driver leaves the card
 * on detach whenever it went past read-only (amdgpu's reset method for MP1
 * 14.0.2/3, amdgpu_device_mode1_reset + smu_v14_0_2_mode1_reset):
 * save config space, bus master off, debug message Mode1Reset, 1 s with no
 * MMIO, restore config space, wait for the PSP bootloader and for the
 * memory size register to read back.
 */
int
amd_smu_mode1_reset(struct atrium_amd_softc *sc)
{
	const struct smu_gen *g = smu_gen_find(sc);
	bus_size_t lim = rman_get_size(sc->regs), memsize;
	uint32_t sos = 0, v = 0;
	int us, err;

	if (g == NULL || g->dbg_mode1_reset == 0 || sc->ip.mp1.nbase < 2 ||
	    smu_reg(sc, g->reg_dbg_msg) + 4 > lim ||
	    smu_reg(sc, g->reg_dbg_param) + 4 > lim ||
	    smu_reg(sc, g->reg_dbg_resp) + 4 > lim ||
	    !sc->ip.nbif.present || sc->ip.nbif.nbase < 3) {
		device_printf(sc->dev, "smu: no mode 1 reset known for SMU "
		    "%u.%u.%u\n", sc->ip.mp1.major, sc->ip.mp1.minor,
		    sc->ip.mp1.rev);
		return (ENOTSUP);
	}
	memsize = ((bus_size_t)sc->ip.nbif.base[2] + 0xc3) * 4; /* RCC_CONFIG_MEMSIZE */
	if (memsize + 4 > lim)
		return (ENOTSUP);

	device_printf(sc->dev, "smu: mode 1 reset (card back to its "
	    "post-POST state)\n");
	pci_save_state(sc->dev);
	pci_disable_busmaster(sc->dev);
	amd_mmio_write32(sc, smu_reg(sc, g->reg_dbg_param), 0);
	amd_mmio_write32(sc, smu_reg(sc, g->reg_dbg_msg), g->dbg_mode1_reset);
	amd_mmio_write32(sc, smu_reg(sc, g->reg_dbg_resp), 0);
	/* No MMIO while the ASIC resets. */
	pause("smurst", hz);
	pci_restore_state(sc->dev);

	err = amd_psp_wait_bootloader(sc, &sos);
	for (us = 0; err == 0 && us < 2000000; us += 100) {
		v = amd_mmio_read32(sc, memsize);
		if (v != 0xffffffff)
			break;
		DELAY(100);
	}
	if (err == 0 && v == 0xffffffff)
		err = ETIMEDOUT;
	/* Cold is proven too (spec A3.9): no sOS, GC off. */
	if (err == 0 && (sos != 0 || (gc_ok(sc, 0, GC12_GB_ADDR_CONFIG) &&
	    gc_rd(sc, 0, GC12_GB_ADDR_CONFIG) != 0)))
		err = EIO;
	device_printf(sc->dev, "smu: after reset: %s; sOS %#x, memsize %u "
	    "MiB, SMU response %#x, GB_ADDR_CONFIG %#x\n",
	    err == 0 ? "PSP bootloader ready, card cold" : "card NOT back "
	    "to cold (needs a power cycle)", sos, v,
	    amd_mmio_read32(sc, smu_reg(sc, g->reg_resp)),
	    gc_ok(sc, 0, GC12_GB_ADDR_CONFIG) ?
	    gc_rd(sc, 0, GC12_GB_ADDR_CONFIG) : 0);
	return (err);
}

/* ---------------------------------------------------------------------------
 * Stage 5a: the driver attached on silicon (docs/spec/r9700-bare-metal.md
 * §4.5). Attach brings the card up (stages 3-4) and leaves it running: SMU,
 * GART, GFX, IH, MES with a kernel compute queue. GfxOff is allowed while
 * nothing is queued (nothing is, yet). A watcher samples telemetry every
 * 5 s and, if the card is not idle three times running or the hotspot
 * reaches SI_HOT_C, brings everything down and resets the card (A3.7). Detach
 * proves MES survived the time attached (the 4e dispatch again), takes
 * everything down, proves idle, then the mode 1 reset (A3.6).
 */
#define SI_WATCH_SECS		5
#define SI_LOG_EVERY		6	/* log every 6th sample: 30 s */
#define SI_HOT_C		95
#define SI_BUSY_STRIKES		3

struct amd_si {
	struct atrium_amd_softc *sc;
	struct smu_link link;
	struct resource *vram;
	int vram_rid;
	uint64_t table_mc;
	uint32_t ifver;
	struct amd_gart gt;
	bool gart_up;
	struct amd_ih7 *ih;
	struct mes *mes;
	struct sx lock;			/* one SMU sender (A3.5) */
	struct timeout_task watch;
	bool stopping, lost;
	bool dma_off;		/* lost: bus mastering off for good */
	u_int nsample, strikes;
	u_int gfx_holds;		/* GfxOff disallowed while > 0 (5c-2) */
	bool client;			/* the client node is attached (5c-2) */
};

/* Everything down to the GART, GfxOff disallowed meanwhile; then allowed. */
static void
si_down(struct amd_si *si, bool selftest)
{
	struct atrium_amd_softc *sc = si->sc;
	int us;

	(void)smu_send(&si->link, SMU_DISALLOW_GFXOFF, 0, NULL, false);
	for (us = 0; us < GC_WAIT_US && gc_rd(sc, 0, GC12_GB_ADDR_CONFIG) == 0;
	    us += 100)
		DELAY(100);
	if (si->mes != NULL) {
		if (selftest)
			device_printf(sc->dev, "si: dispatch after %u s "
			    "attached: %s\n", si->nsample * SI_WATCH_SECS,
			    amd_mes12_selftest(si->mes) == 0 ? "PASS" :
			    "FAILED");
		if (si->ih != NULL)
			device_printf(sc->dev, "si: %d CP EOP IV(s) since "
			    "attach, %u interrupt(s)\n",
			    amd_ih7_drain_eop(si->ih, 8),
			    amd_ih7_nintr(si->ih));
		amd_mes12_down(si->mes);
		si->mes = NULL;
	}
	amd_ih7_down(si->ih);
	si->ih = NULL;
	if (si->gart_up) {
		amd_gmc12_gart_disable(&si->gt);
		si->gart_up = false;
	}
	(void)smu_send(&si->link, SMU_ALLOW_GFXOFF, 0, NULL, false);
}

static void
si_watch(void *arg, int pending __unused)
{
	struct amd_si *si = arg;
	struct atrium_amd_softc *sc = si->sc;
	struct atrium_gpu_power_telemetry t;
	bool ok, hot = false, busy = false, log;

	sx_xlock(&si->lock);
	if (si->stopping || si->lost) {
		sx_xunlock(&si->lock);
		return;
	}
	si->nsample++;
	log = si->nsample % SI_LOG_EVERY == 0;
	ok = smu_metrics(&si->link, si->vram, si->ifver, false, &t);
	if (ok) {
		hot = t.hotspot_c != 0xffff && t.hotspot_c >= SI_HOT_C;
		/* Clients' VMs exist (gfx_holds): work may be running. */
		busy = si->gfx_holds == 0 &&
		    atrium_gpu_power_idle_suspicious(&t);
		si->strikes = busy ? si->strikes + 1 : 0;
	}
	if (log || !ok || hot || busy)
		device_printf(sc->dev, "si: watch #%u: %s%s%s\n", si->nsample,
		    ok ? "telemetry ok" : "no telemetry", hot ? ", HOT" : "",
		    busy ? ", busy with no work queued" : "");
	if (si->link.failed || hot || si->strikes >= SI_BUSY_STRIKES) {
		device_printf(sc->dev, "si: %s; taking the GPU down and "
		    "resetting it (A3.7)\n", si->link.failed ? "SMU link "
		    "failed" : hot ? "hotspot at the limit" : "not idle "
		    "with no work queued");
		/*
		 * With the client node attached its queue and IH consumer
		 * still point at the MES and IH blocks: reset the card
		 * without freeing them (the client refuses all work once
		 * lost; the blocks are leaked at detach).
		 */
		if (!si->link.failed && !si->client)
			si_down(si, false);
		(void)amd_smu_mode1_reset(sc);
		si->lost = true;
		/* Nothing of the card's old state may DMA again. */
		pci_disable_busmaster(sc->dev);
		si->dma_off = true;
	} else
		taskqueue_enqueue_timeout(taskqueue_thread, &si->watch,
		    SI_WATCH_SECS * hz);
	sx_xunlock(&si->lock);
}

int
amd_si_attach(struct atrium_amd_softc *sc)
{
	struct amd_si *si;
	const char *step = "SMU link";
	int err, us, vm_test = 0, uq_test = 0;

	si = malloc(sizeof(*si), M_DEVBUF, M_WAITOK | M_ZERO);
	si->sc = sc;
	sx_init(&si->lock, "atrium-gpu-si");
	TIMEOUT_TASK_INIT(taskqueue_thread, &si->watch, 0, si_watch, si);

	err = smu_open(sc, &si->link, &si->vram, &si->vram_rid,
	    &si->table_mc);
	if (err != 0)
		goto fail;
	step = "SMU start and idle proof";
	err = smu_start(&si->link, si->vram, si->table_mc, &si->ifver);
	if (err != 0)
		goto fail;

	/* Stages 4a-4e, GfxOff held off while we set GC up. */
	(void)smu_send(&si->link, SMU_DISALLOW_GFXOFF, 0, NULL, false);
	for (us = 0; us < GC_WAIT_US && gc_rd(sc, 0, GC12_GB_ADDR_CONFIG) == 0;
	    us += 100)
		DELAY(100);
	step = "GART (4a)";
	if ((err = amd_gmc12_gart_enable(sc, si->vram, &si->gt)) != 0)
		goto down;
	si->gart_up = true;
	step = "GFX setup (4c)";
	if ((err = amd_gfx12_setup(sc)) != 0)
		goto down;
	step = "IH (4b)";
	if ((err = amd_ih7_up(sc, &si->gt, &si->ih)) != 0)
		goto down;
	step = "MES (4d)";
	if ((err = amd_mes12_up(sc, &si->gt, true, &si->mes)) != 0)
		goto down;
	step = "dispatch (4e)";
	err = amd_mes12_selftest(si->mes);
	device_printf(sc->dev, "si: dispatch at attach: %s; %d CP EOP IV(s)\n",
	    err == 0 ? "PASS" : "FAILED", amd_ih7_drain_eop(si->ih, 8));
	if (err != 0)
		goto down;
	TUNABLE_INT_FETCH("hw.atrium_gpu.vm_test", &vm_test);
	if (vm_test) {
		step = "per-client page table (5b)";
		err = amd_mes12_vmtest(si->mes);
		device_printf(sc->dev, "si: VM test at attach: %s; %d CP EOP "
		    "IV(s)\n", err == 0 ? "PASS" : "FAILED",
		    amd_ih7_drain_eop(si->ih, 8));
		if (err != 0)
			goto down;
	}
	TUNABLE_INT_FETCH("hw.atrium_gpu.uq_test", &uq_test);
	if (uq_test) {
		step = "MES user queues (5c-1)";
		err = amd_mes12_uqtest(si->mes);
		device_printf(sc->dev, "si: user-queue test at attach: %s; %d "
		    "CP EOP IV(s)\n", err == 0 ? "PASS" : "FAILED",
		    amd_ih7_drain_eop(si->ih, 8));
		if (err != 0)
			goto down;
	}
	(void)smu_send(&si->link, SMU_ALLOW_GFXOFF, 0, NULL, false);
	step = "idle proof while attached";
	if (!smu_sample(&si->link, si->vram, si->ifver, 5,
	    "attached, MES up, no work")) {
		device_printf(sc->dev, "si: could not prove the card idle "
		    "while attached (A3.9)\n");
		err = EIO;
		goto down;
	}
	step = "SMU link";
	if (si->link.failed) {
		err = EIO;
		goto down;
	}
	sc->si = si;
	taskqueue_enqueue_timeout(taskqueue_thread, &si->watch,
	    SI_WATCH_SECS * hz);
	device_printf(sc->dev, "si: stage 5a: attached and idle (MES up, "
	    "kernel compute queue ready); watching telemetry every %d s\n",
	    SI_WATCH_SECS);
	return (0);

down:
	si_down(si, false);
fail:
	device_printf(sc->dev, "si: stage 5a: attach failed at %s (%d)\n",
	    step, err);
	if (si->vram != NULL)
		bus_release_resource(sc->dev, SYS_RES_MEMORY, si->vram_rid,
		    si->vram);
	sx_destroy(&si->lock);
	free(si, M_DEVBUF);
	return (err);
}

/*
 * Stage 5c-2: what the client node (si_client.c, in the gpu module) uses.
 * GC registers read 0 and drop writes while GfxOff is in effect, so the
 * client node holds GfxOff off while it has VMs (whose GFXHUB contexts,
 * fault status and wave kills are GC registers) and around its own
 * register work; the last release allows GfxOff again.
 */
struct amd_gart *
amd_si_gart(struct atrium_amd_softc *sc)
{
	return (sc->si != NULL ? &sc->si->gt : NULL);
}

/*
 * 5d: the VRAM allocator's range. Below AMD_SI_VRAM_DRIVER_END is the
 * driver's own fixed area (PSP staging at 64 MiB, IP firmware at 68, the
 * SMU table at 84, the GART table at 88, the page-table pool at 92). From
 * the lowest PSP reservation up is the PSP's, and the top of VRAM also
 * holds what the VBIOS and memory training keep (amdgpu reserves those
 * near the top too), so the range ends at the lowest reservation; with
 * none reported, 1 GiB below the top. Silicon need not wipe VRAM at a
 * reset, so the whole range is scrubbed before it is first handed out.
 */
int
amd_si_vram_init(struct atrium_amd_softc *sc)
{
	struct amd_gart *gt = amd_si_gart(sc);
	uint64_t device, lo = AMD_SI_VRAM_DRIVER_END, hi;
	bool reported = false;
	u_int i;

	if (gt == NULL || gt->fb_end <= gt->fb_start)
		return (ENXIO);
	device = gt->fb_end - gt->fb_start + 1;
	hi = device;
	for (i = 0; i < nitems(sc->psp_resv_off); i++) {
		if (sc->psp_resv_size[i] == 0)
			continue;
		reported = true;
		hi = MIN(hi, sc->psp_resv_off[i]);
	}
	if (!reported)
		hi = device > (1ull << 30) ? device - (1ull << 30) : 0;
	hi &= ~((2ull << 20) - 1);
	if (hi <= lo + (64ull << 20)) {
		device_printf(sc->dev, "VRAM: no room for the allocator "
		    "(device %ju MiB, end %#jx)\n", (uintmax_t)(device >> 20),
		    (uintmax_t)hi);
		return (ENOSPC);
	}
	if (!reported)
		device_printf(sc->dev, "VRAM: the PSP reported no "
		    "reservation; keeping the top 1 GiB out\n");
	return (amd_vram_init_range(sc, device, lo, hi, true));
}

struct mes *
amd_si_mes(struct atrium_amd_softc *sc)
{
	return (sc->si != NULL ? sc->si->mes : NULL);
}

struct amd_ih7 *
amd_si_ih(struct atrium_amd_softc *sc)
{
	return (sc->si != NULL ? sc->si->ih : NULL);
}

/* Up, and not taken down by the watch (A3.7) or a failed SMU link. */
void
amd_si_client_attached(struct atrium_amd_softc *sc, bool on)
{
	struct amd_si *si = sc->si;

	if (si == NULL)
		return;
	sx_xlock(&si->lock);
	si->client = on;
	sx_xunlock(&si->lock);
}

/*
 * The last rung (5c-2b): the client side found the card in a state it
 * cannot undo (a user queue MES will not remove). Reset the card (mode 1),
 * mark it lost, and turn bus mastering off, so nothing the old state
 * pointed at can be read or written by the GPU again: everything the
 * driver kept for it may then be freed. Idempotent. Every client queue is
 * lost; detach proceeds normally.
 */
void
amd_si_lose(struct atrium_amd_softc *sc, const char *why)
{
	struct amd_si *si = sc->si;

	if (si == NULL)
		return;
	sx_xlock(&si->lock);
	if (!si->lost) {
		device_printf(sc->dev, "si: %s; resetting the GPU, every "
		    "client queue is lost\n", why);
		if (!si->link.failed)
			(void)amd_smu_mode1_reset(sc);
		si->lost = true;
	}
	if (!si->dma_off) {
		pci_disable_busmaster(sc->dev);
		si->dma_off = true;
	}
	sx_xunlock(&si->lock);
}

bool
amd_si_usable(struct atrium_amd_softc *sc)
{
	struct amd_si *si = sc->si;

	return (si != NULL && !si->lost && !si->stopping && !si->link.failed);
}

int
amd_si_gfx_hold(struct atrium_amd_softc *sc)
{
	struct amd_si *si = sc->si;
	int err = 0, us;

	if (si == NULL)
		return (ENXIO);
	sx_xlock(&si->lock);
	if (si->lost || si->link.failed) {
		sx_xunlock(&si->lock);
		return (EIO);
	}
	if (si->gfx_holds++ == 0) {
		err = smu_send(&si->link, SMU_DISALLOW_GFXOFF, 0, NULL,
		    false) == SMU_RESP_OK ? 0 : EIO;
		for (us = 0; err == 0 && us < GC_WAIT_US &&
		    gc_rd(sc, 0, GC12_GB_ADDR_CONFIG) == 0; us += 100)
			DELAY(100);
		if (err == 0 && us >= GC_WAIT_US)
			err = ETIMEDOUT;
		if (err != 0) {
			device_printf(sc->dev, "si: DisallowGfxOff failed "
			    "(%d)\n", err);
			si->gfx_holds--;
		}
	}
	sx_xunlock(&si->lock);
	return (err);
}

void
amd_si_gfx_release(struct atrium_amd_softc *sc)
{
	struct amd_si *si = sc->si;

	if (si == NULL)
		return;
	sx_xlock(&si->lock);
	KASSERT(si->gfx_holds > 0, ("si: GfxOff hold underflow"));
	if (--si->gfx_holds == 0 && !si->lost && !si->link.failed)
		(void)smu_send(&si->link, SMU_ALLOW_GFXOFF, 0, NULL, false);
	sx_xunlock(&si->lock);
}

/* Detach: everything down, idle proven, the caller resets the card. */
void
amd_si_detach(struct atrium_amd_softc *sc)
{
	struct amd_si *si = sc->si;

	if (si == NULL)
		return;
	sx_xlock(&si->lock);
	si->stopping = true;
	sx_xunlock(&si->lock);
	while (taskqueue_cancel_timeout(taskqueue_thread, &si->watch,
	    NULL) != 0)
		taskqueue_drain_timeout(taskqueue_thread, &si->watch);
	if (!si->lost && !si->link.failed) {
		si_down(si, true);
		if (smu_sample(&si->link, si->vram, si->ifver, 5,
		    "detached"))
			device_printf(sc->dev, "si: card proven idle at "
			    "detach\n");
		else
			device_printf(sc->dev, "si: card NOT idle at detach\n");
	} else
		device_printf(sc->dev, "si: detach after the GPU was taken "
		    "down (%s)\n", si->lost ? "watcher reset it" :
		    "SMU link failed");
	if (si->mes != NULL || si->ih != NULL)
		device_printf(sc->dev, "si: MES/IH blocks of a card reset "
		    "under the client node are leaked, not freed\n");
	bus_release_resource(sc->dev, SYS_RES_MEMORY, si->vram_rid, si->vram);
	sc->si = NULL;
	sx_destroy(&si->lock);
	free(si, M_DEVBUF);
	device_printf(sc->dev, "si: stage 5a: detached\n");
}
