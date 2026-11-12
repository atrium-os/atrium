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

/* `n` telemetry samples 1 s apart, labelled; warn on a busy idle card. */
static void
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
		if (smu_metrics(l, vram, ifver, false, &t) &&
		    atrium_gpu_power_idle_suspicious(&t))
			device_printf(l->sc->dev, "smu: WARNING: busy or at "
			    "the power limit with no work queued\n");
	}
}

int
amd_smu_init(struct atrium_amd_softc *sc)
{
	struct resource *vram = NULL;
	uint64_t fb_base, mc;
	struct smu_link link = { .sc = sc, .failed = false };
	struct atrium_gpu_power_telemetry t;
	uint64_t feat;
	uint32_t v, ifver = 0;
	bus_size_t lim = rman_get_size(sc->regs);
	int rid, us, err = 0, gc_up_ok = 0, gfx_cg = 0;

#define	SEND(m, a, o)	smu_send(&link, (m), (a), (o), false)
	link.gen = smu_gen_find(sc);
	if (link.gen == NULL || sc->ip.mp1.nbase < 2 ||
	    smu_reg(sc, link.gen->reg_msg) + 4 > lim ||
	    smu_reg(sc, link.gen->reg_arg) + 4 > lim ||
	    smu_reg(sc, link.gen->reg_resp) + 4 > lim) {
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
		err = ENXIO;
		goto out;
	}
	fb_base = (uint64_t)(amd_mmio_read32(sc,
	    ((bus_size_t)sc->ip.mmhub.base[0] + MMHUB4_FB_LOCATION_BASE) * 4) &
	    MMHUB4_FB_BASE_MASK) << 24;
	mc = fb_base + SMU_TABLE_VRAM_OFF;

	device_printf(sc->dev, "smu: SMU %u.%u.%u, response register %#x\n",
	    sc->ip.mp1.major, sc->ip.mp1.minor, sc->ip.mp1.rev,
	    amd_mmio_read32(sc, smu_reg(sc, link.gen->reg_resp)));
	gc_report(sc, "before");

	if (SEND(SMU_TEST, 0, &v) != SMU_RESP_OK ||
	    SEND(SMU_GET_VERSION, 0, &v) != SMU_RESP_OK ||
	    SEND(SMU_GET_DRIVER_IF, 0, &ifver) != SMU_RESP_OK) {
		err = EIO;
		goto out;
	}
	bus_set_region_4(vram, SMU_TABLE_VRAM_OFF, 0, SMU_TABLE_SIZE / 4);
	if (SEND(SMU_SET_DRIVER_ADDR_HI, (uint32_t)(mc >> 32), NULL) !=
	    SMU_RESP_OK ||
	    SEND(SMU_SET_DRIVER_ADDR_LO, (uint32_t)mc, NULL) != SMU_RESP_OK) {
		err = EIO;
		goto out;
	}
	feat = smu_features(&link);
	if (link.failed) {
		err = EIO;
		goto out;
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
		goto out;
	}
	if (feat == 0) {
		/* First start this boot: amdgpu's smc_hw_setup order. */
		if (SEND(SMU_RUN_DC_BTC, 0, NULL) != SMU_RESP_OK) {
			err = EIO;
			goto out;
		}
		/*
		 * Under SCPM the PSP owns the feature mask and the PMFW
		 * refuses the driver's with 0xFD; amdgpu skips it then.
		 */
		v = smu_send(&link, SMU_SET_ALLOWED_HI, 0xffffffff, NULL, true);
		if (v == SMU_RESP_OK)
			v = smu_send(&link, SMU_SET_ALLOWED_LO, 0xffffffff,
			    NULL, true);
		if (v == SMU_RESP_BAD_PREREQ)
			device_printf(sc->dev, "smu: allowed mask refused: "
			    "SCPM, the PSP owns it\n");
		if (link.failed ||
		    SEND(SMU_ENABLE_ALL, 0, NULL) != SMU_RESP_OK) {
			err = EIO;
			goto out;
		}
		feat = smu_features(&link);
	} else
		device_printf(sc->dev, "smu: features already running this "
		    "boot; not enabling again\n");
	if ((feat & link.gen->required) != link.gen->required) {
		device_printf(sc->dev, "smu: required protection features "
		    "missing (%#jx of %#jx); refusing the GPU\n",
		    (uintmax_t)(feat & link.gen->required),
		    (uintmax_t)link.gen->required);
		err = EIO;
		goto out;
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
	(void)smu_metrics(&link, vram, ifver, true, &t);
	pause("smumet", hz);
	smu_sample(&link, vram, ifver, 1, "RLC up, no gfx init");

	/*
	 * Power P2 step 2 experiment (kenv hw.atrium_gpu.gfx_cg=1): the GC
	 * idle setup amdgpu does after RLC boot, a step at a time with
	 * telemetry after each: SRM, clock gating, then GfxOff allowed.
	 */
	TUNABLE_INT_FETCH("hw.atrium_gpu.gfx_cg", &gfx_cg);
	if (gfx_cg && !link.failed) {
		amd_gfx12_pm_report(sc, "before");
		device_printf(sc->dev, "gfx: SRM enable: %d\n",
		    amd_gfx12_srm_enable(sc));
		smu_sample(&link, vram, ifver, 2, "SRM on");
		device_printf(sc->dev, "gfx: clock gating on: %d\n",
		    amd_gfx12_clock_gating(sc, true));
		amd_gfx12_pm_report(sc, "after CG");
		smu_sample(&link, vram, ifver, 3, "clock gating on");
		(void)SEND(SMU_ALLOW_GFXOFF, 0, NULL);
		smu_sample(&link, vram, ifver, 3, "GfxOff allowed");
		amd_gfx12_pm_report(sc, "end");
	}

	/* Hand back the PMFW's own idle policy (rule 6). */
	(void)SEND(SMU_ALLOW_GFXOFF, 0, NULL);
	if (link.failed)
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
	device_printf(sc->dev, "smu: after reset: %s; sOS %#x, memsize %u "
	    "MiB, SMU response %#x, GB_ADDR_CONFIG %#x\n",
	    err == 0 ? "PSP bootloader ready" : "card did NOT come back "
	    "(needs a power cycle)", sos, v,
	    amd_mmio_read32(sc, smu_reg(sc, g->reg_resp)),
	    gc_ok(sc, 0, GC12_GB_ADDR_CONFIG) ?
	    gc_rd(sc, 0, GC12_GB_ADDR_CONFIG) : 0);
	return (err);
}
