/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * identify.c — what is behind BAR5? Decided once at attach, before bus
 * mastering and before any register write except the discovery window's.
 *
 *  - gpusim answers its identity register (regSIM_ID reads 'GPUS'). It models
 *    one part, and that part's versions are written down here as the model's.
 *  - Silicon describes itself: the IP discovery table at the top of VRAM gives
 *    every block's version and register segment bases (base/base_rs.rs parses
 *    it; the bytes are untrusted). The ISA name comes from GC's version and
 *    the display segment from DMU's bases. Nothing here knows a PCI device id
 *    (Shankar 2026-11-05: never hardcode RDNA4; pick code by IP version).
 *
 * On silicon the only writes are MM_INDEX / MM_INDEX_HI, the indirect VRAM
 * window, the same reads amdgpu does before it owns anything.
 */
#include "atrium_gpu_amd.h"
#include "atrium_gpu_amd_abi.h"

/* SOC15 fixed offsets, needed before discovery (bytes in BAR5). */
#define MM_INDEX		0x0
#define MM_DATA			0x4
#define MM_INDEX_HI		0x18
#define RCC_CONFIG_MEMSIZE	(0xde3 * 4)	/* VRAM size in MiB */
/* Where the discovery binary sits, counted back from the end of VRAM. */
#define DISCOVERY_OFFSET_FROM_TOP	(64u << 10)
#define DISCOVERY_SIZE		(10u << 10)

static MALLOC_DEFINE(M_ATRIUM_DISC, "atrium_disc", "IP discovery copy");

/* ISA from GC's version: gfx<major><minor><rev>, family by GC major. */
static void
amd_isa_from_gc(struct atrium_amd_softc *sc)
{
	const struct amd_ip_ver *gc = &sc->ip.gc;

	switch (gc->major) {
	case 12:
		sc->isa_family = ATRIUM_GPU_ISA_AMD_RDNA4;
		break;
	case 11:
		sc->isa_family = ATRIUM_GPU_ISA_AMD_RDNA3;
		break;
	default:
		sc->isa_family = 0;	/* no compiler target: CAP_ISA says so */
		break;
	}
	/* The name's digits as hex, as CAP_ISA defines it: gfx1201 -> 0x1201. */
	sc->isa_revision = ((uint32_t)(gc->major / 10) << 12) |
	    ((uint32_t)(gc->major % 10) << 8) | ((gc->minor & 0xf) << 4) |
	    (gc->rev & 0xf);
	snprintf(sc->isa_name, sizeof(sc->isa_name), "gfx%u%x%x",
	    gc->major, gc->minor, gc->rev);
}

static void
amd_ip_print(struct atrium_amd_softc *sc, const char *name,
    const struct amd_ip_ver *v)
{
	if (v->present)
		device_printf(sc->dev, "  %-7s %u.%u.%u (%u segments, base0 %#x)\n",
		    name, v->major, v->minor, v->rev, v->nbase, v->base[0]);
	else
		device_printf(sc->dev, "  %-7s absent\n", name);
}

static int
amd_identify_silicon(struct atrium_amd_softc *sc)
{
	bus_size_t len = rman_get_size(sc->regs);
	uint32_t memsize, *buf;
	uint64_t addr;
	u_int i;
	int err;

	if (len < RCC_CONFIG_MEMSIZE + 4) {
		device_printf(sc->dev, "BAR5 is %ju bytes: not an AMD register "
		    "file we know\n", (uintmax_t)len);
		return (ENXIO);
	}
	memsize = amd_mmio_read32(sc, RCC_CONFIG_MEMSIZE);
	if (memsize == 0 || memsize == 0xffffffff ||
	    ((uint64_t)memsize << 20) < DISCOVERY_OFFSET_FROM_TOP) {
		device_printf(sc->dev, "VRAM size unreadable (%#x); not touching "
		    "the VRAM window\n", memsize);
		return (ENXIO);
	}

	buf = malloc(DISCOVERY_SIZE, M_ATRIUM_DISC, M_WAITOK | M_ZERO);
	addr = ((uint64_t)memsize << 20) - DISCOVERY_OFFSET_FROM_TOP;
	for (i = 0; i < DISCOVERY_SIZE / 4; i++, addr += 4) {
		amd_mmio_write32(sc, MM_INDEX,
		    ((uint32_t)addr & 0x7fffffff) | 0x80000000);
		amd_mmio_write32(sc, MM_INDEX_HI, (uint32_t)(addr >> 31));
		buf[i] = amd_mmio_read32(sc, MM_DATA);
	}
	err = atrium_amd_discovery_parse((const uint8_t *)buf, DISCOVERY_SIZE,
	    &sc->ip);
	free(buf, M_ATRIUM_DISC);
	if (err != 0) {
		device_printf(sc->dev, "IP discovery table rejected (%d): not a "
		    "part we can describe\n", err);
		return (ENXIO);
	}

	sc->hw_kind = AMD_HW_SILICON;
	device_printf(sc->dev, "silicon: %u MiB VRAM, %u IP instances\n",
	    memsize, sc->ip.nips);
	amd_ip_print(sc, "GC", &sc->ip.gc);
	amd_ip_print(sc, "PSP", &sc->ip.mp0);
	amd_ip_print(sc, "SMU", &sc->ip.mp1);
	amd_ip_print(sc, "SDMA", &sc->ip.sdma0);
	amd_ip_print(sc, "MMHUB", &sc->ip.mmhub);
	amd_ip_print(sc, "OSSSYS", &sc->ip.osssys);
	amd_ip_print(sc, "HDP", &sc->ip.hdp);
	amd_ip_print(sc, "NBIF", &sc->ip.nbif);
	amd_ip_print(sc, "DCN", &sc->ip.dmu);
	amd_ip_print(sc, "VCN", &sc->ip.vcn);
	if (!sc->ip.gc.present) {
		device_printf(sc->dev, "no GC block: not a GPU we drive\n");
		return (ENXIO);
	}
	if (sc->ip.dmu.present && sc->ip.dmu.nbase > 2)
		sc->dcn_seg2_bytes = sc->ip.dmu.base[2] * 4;
	return (0);
}

/*
 * The part gpusim models (Navi 48: GC 12.0.1, DCN 4.0.1 with segment 2 at
 * byte 0x60000 of its register BAR). This is the simulator's description of
 * itself, standing in for the discovery table it does not serve.
 */
static void
amd_identify_gpusim(struct atrium_amd_softc *sc)
{
	sc->hw_kind = AMD_HW_GPUSIM;
	sc->ip.gc = (struct amd_ip_ver){ .present = 1, .major = 12, .minor = 0, .rev = 1 };
	sc->ip.dmu = (struct amd_ip_ver){ .present = 1, .major = 4, .minor = 0, .rev = 1 };
	sc->dcn_seg2_bytes = DCN_SEG2_BYTES_GPUSIM;
}

int
amd_identify(struct atrium_amd_softc *sc)
{
	int err;

	memset(&sc->ip, 0, sizeof(sc->ip));
	if (amd_mmio_read32(sc, regSIM_ID) == SIM_ID_MAGIC) {
		amd_identify_gpusim(sc);
		err = 0;
	} else {
		err = amd_identify_silicon(sc);
	}
	if (err != 0)
		return (err);
	amd_isa_from_gc(sc);
	device_printf(sc->dev, "%s, ISA %s\n",
	    sc->hw_kind == AMD_HW_GPUSIM ? "gpusim" : "silicon", sc->isa_name);
	return (0);
}
