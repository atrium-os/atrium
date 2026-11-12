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

/* MP1 14.x mailbox, dword offsets in MP1 segment 1. */
#define SMU14_C2PMSG_66		0x82	/* message */
#define SMU14_C2PMSG_82		0x92	/* argument / output */
#define SMU14_C2PMSG_90		0x9a	/* response */
#define SMU_RESP_OK		0x01
#define SMU_RESP_BAD_PREREQ	0xfd
#define SMU_WAIT_US		2000000

/*
 * The allowlist (docs/spec/gpu-power-management.md §2 rule 1, §3): the only
 * messages this driver can express. smu_send() takes an index into this
 * table, never a raw id; adding a row is a spec change. Ids from
 * smu_v14_0_2_ppsmc.h.
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
static const struct {
	uint16_t id;
	const char *name;
} smu_msgs[SMU_NMSG] = {
	[SMU_TEST] =		{ 0x01, "TestMessage" },
	[SMU_GET_VERSION] =	{ 0x02, "GetSmuVersion" },
	[SMU_GET_DRIVER_IF] =	{ 0x03, "GetDriverIfVersion" },
	[SMU_SET_DRIVER_ADDR_HI] = { 0x0e, "SetDriverDramAddrHigh" },
	[SMU_SET_DRIVER_ADDR_LO] = { 0x0f, "SetDriverDramAddrLow" },
	[SMU_RUN_DC_BTC] =	{ 0x36, "RunDcBtc" },
	[SMU_SET_ALLOWED_HI] =	{ 0x05, "SetAllowedFeaturesMaskHigh" },
	[SMU_SET_ALLOWED_LO] =	{ 0x04, "SetAllowedFeaturesMaskLow" },
	[SMU_ENABLE_ALL] =	{ 0x06, "EnableAllSmuFeatures" },
	[SMU_GET_RUNNING_LO] =	{ 0x0c, "GetRunningSmuFeaturesLow" },
	[SMU_GET_RUNNING_HI] =	{ 0x0d, "GetRunningSmuFeaturesHigh" },
	[SMU_TABLE_SMU2DRAM] =	{ 0x12, "TransferTableSmu2Dram" },
	[SMU_ALLOW_GFXOFF] =	{ 0x28, "AllowGfxOff" },
	[SMU_DISALLOW_GFXOFF] =	{ 0x29, "DisallowGfxOff" },
};
#define SMU_TABLE_SMU_METRICS	5

/*
 * Features the PMFW must be running before the GPU is used (spec §4):
 * VR0HOT 27, FW_CTF 28, FAN_CONTROL 29, THROTTLERS 23, FW_DSTATE 17,
 * MEM_TEMP_READ 46 (smu14_driver_if_v14_0.h).
 */
#define SMU_REQUIRED_FEATURES	((1ull << 17) | (1ull << 23) | (1ull << 27) | \
	(1ull << 28) | (1ull << 29) | (1ull << 46))

struct amd_smu_metrics {	/* rs/smu_metrics.rs Metrics */
	uint32_t verified, counter;
	uint16_t temp_c[12];
	uint16_t socket_power_w, board_power_w, fan_rpm;
	uint8_t fan_pwm, gfx_activity;
	uint16_t gfxclk_mhz, memclk_mhz;
	uint32_t clk_mhz[11];
	uint8_t throttling[21], pad[3];
};
int	atrium_amd_smu_metrics(const uint8_t *b, size_t len, uint32_t ifver,
	    struct amd_smu_metrics *out);	/* Rust */
#define SMU_METRICS_LEN		260

/* One sender, one state: after a bad answer nothing more is sent (rule 4). */
struct smu_link {
	struct atrium_amd_softc *sc;
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
smu_poll(struct atrium_amd_softc *sc)
{
	uint32_t v = 0;
	int us;

	for (us = 0; us < SMU_WAIT_US; us += 10) {
		v = amd_mmio_read32(sc, smu_reg(sc, SMU14_C2PMSG_90));
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
	resp = smu_poll(sc);
	if (resp == 0) {
		device_printf(sc->dev, "smu: %s: previous message never "
		    "answered; link failed, PMFW left alone\n", smu_msgs[m].name);
		l->failed = true;
		return (0);
	}
	amd_mmio_write32(sc, smu_reg(sc, SMU14_C2PMSG_90), 0);
	amd_mmio_write32(sc, smu_reg(sc, SMU14_C2PMSG_82), arg);
	amd_mmio_write32(sc, smu_reg(sc, SMU14_C2PMSG_66), smu_msgs[m].id);
	resp = smu_poll(sc);
	if (resp == SMU_RESP_OK && out != NULL)
		*out = amd_mmio_read32(sc, smu_reg(sc, SMU14_C2PMSG_82));
	device_printf(sc->dev, "smu: %-26s arg %#010x: resp %#x%s",
	    smu_msgs[m].name, arg, resp, resp == 0 ? " (timeout)" : "");
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
 * BAR0, hand it to the Rust parser (range-checked; untrusted bytes).
 */
static void
smu_metrics(struct smu_link *l, struct resource *vram, uint32_t ifver,
    bool dump)
{
	struct atrium_amd_softc *sc = l->sc;
	struct amd_smu_metrics m;
	uint8_t b[SMU_METRICS_LEN];
	int r, i;

	if (smu_send(l, SMU_TABLE_SMU2DRAM, SMU_TABLE_SMU_METRICS, NULL,
	    false) != SMU_RESP_OK)
		return;
	bus_read_region_1(vram, SMU_TABLE_VRAM_OFF, b, sizeof(b));
	if (dump) {
		/* Raw once, to check the layout by eye (interface version). */
		for (i = 0; i < SMU_METRICS_LEN; i += 32)
			device_printf(sc->dev, "smu: metrics +%3d: %32D\n", i,
			    b + i, " ");
	}
	r = atrium_amd_smu_metrics(b, sizeof(b), ifver, &m);
	if (r < 0) {
		device_printf(sc->dev, "smu: telemetry unavailable (%s %d; "
		    "driver interface %#x)\n", r == -10000 ? "short read" :
		    "implausible field at byte", r == -10000 ? 0 : -r - 1, ifver);
		return;
	}
	device_printf(sc->dev, "smu: metrics #%u%s: edge %u C, hotspot %u C, "
	    "mem %u C, VR gfx %u C; socket %u W, board %u W; fan %u rpm "
	    "(%u%%); gfx %u MHz, mem %u MHz, gfx busy %u%%\n", m.counter,
	    m.verified ? "" : " (layout unverified for this interface)",
	    m.temp_c[0], m.temp_c[1], m.temp_c[4], m.temp_c[5],
	    m.socket_power_w, m.board_power_w, m.fan_rpm, m.fan_pwm,
	    m.gfxclk_mhz, m.memclk_mhz, m.gfx_activity);
	for (i = 0; i < 21; i++)
		if (m.throttling[i] != 0)
			device_printf(sc->dev, "smu: throttler %d active %u%%\n",
			    i, m.throttling[i]);
}

int
amd_smu_init(struct atrium_amd_softc *sc)
{
	struct resource *vram = NULL;
	uint64_t fb_base, mc;
	struct smu_link link = { .sc = sc, .failed = false };
	uint64_t feat;
	uint32_t v, ifver = 0;
	bus_size_t lim = rman_get_size(sc->regs);
	int rid, us, err = 0;

#define	SEND(m, a, o)	smu_send(&link, (m), (a), (o), false)
	if (!sc->ip.mp1.present || sc->ip.mp1.major != 14 ||
	    sc->ip.mp1.nbase < 2 ||
	    smu_reg(sc, SMU14_C2PMSG_90) + 4 > lim) {
		device_printf(sc->dev, "smu: SMU %u.%u.%u: no mailbox known "
		    "here; not touching it\n", sc->ip.mp1.major,
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
	    amd_mmio_read32(sc, smu_reg(sc, SMU14_C2PMSG_90)));
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
	if ((feat & SMU_REQUIRED_FEATURES) != SMU_REQUIRED_FEATURES) {
		device_printf(sc->dev, "smu: required protection features "
		    "missing (%#jx of %#jx); refusing the GPU\n",
		    (uintmax_t)(feat & SMU_REQUIRED_FEATURES),
		    (uintmax_t)SMU_REQUIRED_FEATURES);
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

	/* Telemetry, read only (spec §4): two reads 1 s apart. */
	smu_metrics(&link, vram, ifver, true);
	pause("smumet", hz);
	smu_metrics(&link, vram, ifver, false);

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
