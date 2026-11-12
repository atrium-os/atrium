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
 * Sequence (smu_hw_init / smu_smc_hw_setup for 14.0.2/3, no SCPM writes):
 * TestMessage, GetSmuVersion, GetDriverIfVersion, SetDriverDramAddr (a VRAM
 * buffer for later table transfers), RunDcBtc, SetAllowedFeaturesMask (all,
 * as smu_v14_0_2_init_allowed_features; refused under SCPM, then skipped as
 * amdgpu does), EnableAllSmuFeatures; then, for
 * bring-up, DisallowGfxOff so GC registers stay readable, and report the RLC.
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

/* smu_v14_0_2_ppsmc.h */
#define PPSMC_MSG_TestMessage			0x01
#define PPSMC_MSG_GetSmuVersion			0x02
#define PPSMC_MSG_GetDriverIfVersion		0x03
#define PPSMC_MSG_SetAllowedFeaturesMaskLow	0x04
#define PPSMC_MSG_SetAllowedFeaturesMaskHigh	0x05
#define PPSMC_MSG_EnableAllSmuFeatures		0x06
#define PPSMC_MSG_GetRunningSmuFeaturesLow	0x0c
#define PPSMC_MSG_GetRunningSmuFeaturesHigh	0x0d
#define PPSMC_MSG_SetDriverDramAddrHigh		0x0e
#define PPSMC_MSG_SetDriverDramAddrLow		0x0f
#define PPSMC_MSG_DisallowGfxOff		0x29
#define PPSMC_MSG_RunDcBtc			0x36

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

/* One message; *out gets C2PMSG_82 after an OK. Returns the response. */
static uint32_t
smu_msg(struct atrium_amd_softc *sc, const char *name, uint32_t msg,
    uint32_t arg, uint32_t *out)
{
	uint32_t resp;

	resp = smu_poll(sc);
	if (resp == 0) {
		device_printf(sc->dev, "smu: %s: previous message never "
		    "answered\n", name);
		return (0);
	}
	amd_mmio_write32(sc, smu_reg(sc, SMU14_C2PMSG_90), 0);
	amd_mmio_write32(sc, smu_reg(sc, SMU14_C2PMSG_82), arg);
	amd_mmio_write32(sc, smu_reg(sc, SMU14_C2PMSG_66), msg);
	resp = smu_poll(sc);
	if (resp == SMU_RESP_OK && out != NULL)
		*out = amd_mmio_read32(sc, smu_reg(sc, SMU14_C2PMSG_82));
	device_printf(sc->dev, "smu: %-24s arg %#010x: resp %#x%s", name, arg,
	    resp, resp == 0 ? " (timeout)" : "");
	if (resp == SMU_RESP_OK && out != NULL)
		printf(", out %#x", *out);
	printf("\n");
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

int
amd_smu_init(struct atrium_amd_softc *sc)
{
	struct resource *vram = NULL;
	uint64_t fb_base, mc;
	uint32_t v, lo, hi;
	bus_size_t lim = rman_get_size(sc->regs);
	int rid, us, err = 0;

#define	MSG(n, a, o)	smu_msg(sc, #n, PPSMC_MSG_##n, (a), (o))
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

	if (MSG(TestMessage, 0, &v) != SMU_RESP_OK ||
	    MSG(GetSmuVersion, 0, &v) != SMU_RESP_OK ||
	    MSG(GetDriverIfVersion, 0, &v) != SMU_RESP_OK) {
		err = EIO;
		goto out;
	}
	bus_set_region_4(vram, SMU_TABLE_VRAM_OFF, 0, SMU_TABLE_SIZE / 4);
	if (MSG(SetDriverDramAddrHigh, (uint32_t)(mc >> 32), NULL) !=
	    SMU_RESP_OK ||
	    MSG(SetDriverDramAddrLow, (uint32_t)mc, NULL) != SMU_RESP_OK ||
	    MSG(RunDcBtc, 0, NULL) != SMU_RESP_OK) {
		err = EIO;
		goto out;
	}
	lo = hi = 0;
	(void)MSG(GetRunningSmuFeaturesLow, 0, &lo);
	(void)MSG(GetRunningSmuFeaturesHigh, 0, &hi);
	device_printf(sc->dev, "smu: features before enable %#010x%08x\n", hi,
	    lo);
	/*
	 * With SCPM the PSP owns the feature mask and the PMFW refuses the
	 * driver's (0xFD, bad prerequisite); amdgpu skips it then
	 * (adev->scpm_enabled). Carry on to EnableAllSmuFeatures either way.
	 */
	v = MSG(SetAllowedFeaturesMaskHigh, 0xffffffff, NULL);
	if (v == SMU_RESP_OK)
		v = MSG(SetAllowedFeaturesMaskLow, 0xffffffff, NULL);
	if (v == SMU_RESP_BAD_PREREQ)
		device_printf(sc->dev, "smu: allowed mask refused: SCPM, the "
		    "PSP owns it\n");
	else if (v != SMU_RESP_OK) {
		err = EIO;
		goto out;
	}
	if (MSG(EnableAllSmuFeatures, 0, NULL) != SMU_RESP_OK) {
		err = EIO;
		goto out;
	}
	lo = hi = 0;
	(void)MSG(GetRunningSmuFeaturesLow, 0, &lo);
	(void)MSG(GetRunningSmuFeaturesHigh, 0, &hi);
	device_printf(sc->dev, "smu: running features %#010x%08x\n", hi, lo);
	(void)MSG(DisallowGfxOff, 0, NULL);

	for (us = 0; us < GC_WAIT_US; us += 100) {
		if ((gc_rd(sc, 1, GC12_RLC_RLCS_BOOTLOAD_STATUS) &
		    GC12_BOOTLOAD_COMPLETE) && gc_rd(sc, 0, GC12_CP_STAT) == 0)
			break;
		DELAY(100);
	}
	device_printf(sc->dev, "smu: RLC autoload %s after %d us\n",
	    us < GC_WAIT_US ? "complete" : "NOT complete", us);
	gc_report(sc, "after");
#undef MSG
out:
	if (vram != NULL)
		bus_release_resource(sc->dev, SYS_RES_MEMORY, rid, vram);
	return (err);
}
