/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * gfx12.c — stage 4c of the silicon bring-up (docs/spec/r9700-bare-metal.md
 * §4.4): the rest of GFX hw_init after the PSP's RLC autoload, up to but not
 * including starting the command processor. The CP stays halted here; MES
 * and the KIQ are unhalted in 4d.
 *
 * Interface: Linux's MIT-licensed gfx_v12_0.c (gfx_v12_0_hw_init with PSP
 * loading: wait for RLC autoload, golden registers, gb_addr_config,
 * config_gfx_rs64, gfxhub fault defaults, constants_init) and soc24.c
 * (soc24_grbm_select). Offsets gc_12_0_0_offset.h, fields
 * gc_12_0_0_sh_mask.h. The clear-state buffer (init_csb) is only used by the
 * graphics ring and comes with it, not here.
 *
 * Per version: gfx_gens[] by GC version. Unknown: not touched (A3.10).
 */
#include "atrium_gpu_amd.h"

#include <sys/firmware.h>
#include <dev/pci/pcivar.h>

/* Segment 0 / segment 1 dword offsets, GC 12.0.x. */
#define S0(x)	(0u << 24 | (x))
#define S1(x)	(1u << 24 | (x))

struct gfx_gen {
	uint8_t major, minor, rev_min, rev_max;
	const char *name;
	uint32_t rlc_bootload_status, grbm_status, cp_stat, gb_addr_config;
	uint32_t grbm_cntl, grbm_gfx_cntl, grbm_gfx_index;
	uint32_t db_mem_config, sh_mem_config, sh_mem_bases, spi_gdbg_vmid;
	uint32_t pfp_start, pfp_start_hi, me_start, me_start_hi;
	uint32_t mec_start, mec_start_hi, cp_me_cntl, cp_mec_rs64_cntl;
	uint32_t cc_rb_disable, user_rb_disable, cc_sa_disable, user_sa_disable;
	uint32_t cp_debug, db_ring_control;
	int me_pipes, mec_pipes;
};

static const struct gfx_gen gfx_gens[] = {
	{ 12, 0, 0, 1, "GC 12.0.0/1",
	  S1(0x4e7c), S0(0x0da4), S0(0x0f40), S0(0x13de),
	  S0(0x0da0), S1(0x0900), S1(0x2200),
	  S0(0x13d2), S1(0x09e4), S1(0x09e3), S0(0x1f72),
	  S0(0x1e44), S0(0x1e59), S0(0x1e45), S0(0x1e79),
	  S1(0x2900), S1(0x2938), S1(0x0803), S1(0x2904),
	  S0(0x13dd), S1(0x5b94), S0(0x0fe9), S1(0x5b92),
	  S0(0x1e1f), S0(0x13bb),
	  2, 4 },
};

#define BOOTLOAD_COMPLETE		0x80000000u
#define GRBM_READ_TIMEOUT_MASK		0x00000fffu
#define GRBM_GFX_INDEX_BROADCAST	0xe0000000u	/* SE, SA, instance */
#define CP_DEBUG_UTCL1_HALT_DISABLE	0x00008000u
#define CP_ME_PFP_PIPE_RESET		0x000c0000u	/* PIPE0 | PIPE1 */
#define CP_ME_ME_PIPE_RESET		0x00300000u
#define CP_ME_HALTS			0x14000000u	/* PFP, ME */
#define CP_MEC_PIPE_RESET		0x000f0000u	/* PIPE0..3 */
#define CP_MEC_HALT			0x40000000u
#define CP_MEC_PIPE_ACTIVE		0x3c000000u	/* PIPE0..3 */
#define CP_MEC_INVALIDATE_ICACHE	0x00000010u
#define DB_MEM_CONFIG_GOLDEN		0x00008000u	/* golden_settings_gc_12_0 */
#define TRAP_EN				0x00000008u
/* DEFAULT_SH_MEM_CONFIG: 64-bit, unaligned, INITIAL_INST_PREFETCH 3. */
#define SH_MEM_CONFIG_DEFAULT		((3u << 2) | (3u << 14))
/* gmc_v12_0: private 0x1000'0000'0000'0000, shared 0x2000'...; >> 48. */
#define SH_MEM_BASES_GFX		((0x2000u << 16) | 0x1000u)
/* init_compute_vmid: LDS_APP_BASE 1 (shared), SCRATCH_APP_BASE 2. */
#define SH_MEM_BASES_COMPUTE		((1u << 16) | 2u)
#define FIRST_COMPUTE_VMID		8
#define NUM_VMID			16

static bus_size_t
gfx_reg(struct atrium_amd_softc *sc, uint32_t r)
{
	return (((bus_size_t)sc->ip.gc.base[r >> 24] + (r & 0xffffff)) * 4);
}

static uint32_t
rd(struct atrium_amd_softc *sc, uint32_t r)
{
	return (amd_mmio_read32(sc, gfx_reg(sc, r)));
}

static void
wr(struct atrium_amd_softc *sc, uint32_t r, uint32_t v)
{
	amd_mmio_write32(sc, gfx_reg(sc, r), v);
}

/* soc24_grbm_select: me 0 = gfx, 1 = MEC. */
static void
grbm_select(struct atrium_amd_softc *sc, const struct gfx_gen *g, u_int me,
    u_int pipe, u_int queue, u_int vmid)
{
	wr(sc, g->grbm_gfx_cntl, (pipe & 3) | (me & 3) << 2 |
	    (vmid & 0xf) << 4 | (queue & 7) << 8);
}

static const struct gfx_gen *
gfx_gen_find(struct atrium_amd_softc *sc)
{
	const struct amd_ip_ver *gc = &sc->ip.gc;
	size_t i;

	for (i = 0; i < nitems(gfx_gens); i++)
		if (gfx_gens[i].major == gc->major &&
		    gfx_gens[i].minor == gc->minor &&
		    gc->rev >= gfx_gens[i].rev_min &&
		    gc->rev <= gfx_gens[i].rev_max)
			return (&gfx_gens[i]);
	return (NULL);
}

static void
gfx_report(struct atrium_amd_softc *sc, const struct gfx_gen *g,
    const char *when)
{
	device_printf(sc->dev, "gfx12: %s: GRBM_STATUS %#x, CP_STAT %#x, "
	    "CP_ME_CNTL %#x, CP_MEC_RS64_CNTL %#x, CP_DEBUG %#x, GRBM_CNTL "
	    "%#x\n", when, rd(sc, g->grbm_status), rd(sc, g->cp_stat),
	    rd(sc, g->cp_me_cntl), rd(sc, g->cp_mec_rs64_cntl),
	    rd(sc, g->cp_debug), rd(sc, g->grbm_cntl));
}

/* The three RS64 program start addresses, from the firmware headers. */
static int
rs64_starts(struct atrium_amd_softc *sc, uint64_t start[3])
{
	static const char *const what[3] = { "pfp", "me", "mec" };
	const struct firmware *fw;
	char name[64];
	int i, r;

	for (i = 0; i < 3; i++) {
		snprintf(name, sizeof(name), "amdgpu/gc_%u_%u_%u_%s.bin",
		    sc->ip.gc.major, sc->ip.gc.minor, sc->ip.gc.rev, what[i]);
		fw = firmware_get_flags(name, FIRMWARE_GET_NOWARN);
		if (fw == NULL) {
			device_printf(sc->dev, "gfx12: no /boot/firmware/%s\n",
			    name);
			return (ENOENT);
		}
		r = atrium_amd_rs64_start(fw->data, fw->datasize, &start[i]);
		firmware_put(fw, FIRMWARE_UNLOAD);
		if (r != 0) {
			device_printf(sc->dev, "gfx12: %s header refused\n",
			    name);
			return (EINVAL);
		}
	}
	device_printf(sc->dev, "gfx12: RS64 start: PFP %#jx, ME %#jx, MEC "
	    "%#jx\n", (uintmax_t)start[0], (uintmax_t)start[1],
	    (uintmax_t)start[2]);
	return (0);
}

/* (hi << 30) | (lo >> 2) and hi >> 2, as gfx_v12_0_config_gfx_rs64. */
static void
rs64_program(struct atrium_amd_softc *sc, uint32_t reg, uint32_t reg_hi,
    uint64_t start)
{
	uint32_t lo = (uint32_t)start, hi = (uint32_t)(start >> 32);

	wr(sc, reg, (hi << 30) | (lo >> 2));
	wr(sc, reg_hi, hi >> 2);
}

static void
config_rs64(struct atrium_amd_softc *sc, const struct gfx_gen *g,
    const uint64_t start[3])
{
	uint32_t t;
	int p;

	for (p = 0; p < g->me_pipes; p++) {
		grbm_select(sc, g, 0, p, 0, 0);
		rs64_program(sc, g->pfp_start, g->pfp_start_hi, start[0]);
	}
	grbm_select(sc, g, 0, 0, 0, 0);
	t = rd(sc, g->cp_me_cntl);
	wr(sc, g->cp_me_cntl, t | CP_ME_PFP_PIPE_RESET);
	wr(sc, g->cp_me_cntl, t & ~CP_ME_PFP_PIPE_RESET);

	for (p = 0; p < g->me_pipes; p++) {
		grbm_select(sc, g, 0, p, 0, 0);
		rs64_program(sc, g->me_start, g->me_start_hi, start[1]);
	}
	grbm_select(sc, g, 0, 0, 0, 0);
	t = rd(sc, g->cp_me_cntl);
	wr(sc, g->cp_me_cntl, t | CP_ME_ME_PIPE_RESET);
	wr(sc, g->cp_me_cntl, t & ~CP_ME_ME_PIPE_RESET);

	for (p = 0; p < g->mec_pipes; p++) {
		grbm_select(sc, g, 1, p, 0, 0);
		rs64_program(sc, g->mec_start, g->mec_start_hi, start[2]);
	}
	grbm_select(sc, g, 0, 0, 0, 0);
	t = rd(sc, g->cp_mec_rs64_cntl);
	wr(sc, g->cp_mec_rs64_cntl, t | CP_MEC_PIPE_RESET);
	wr(sc, g->cp_mec_rs64_cntl, t & ~CP_MEC_PIPE_RESET);
}

/*
 * SH_MEM per VMID (constants_init + init_compute_vmid): VMID 0-7 graphics
 * apertures, 8-15 compute apertures with the debug trap on. Which VMIDs
 * the driver hands to apps is decided with the first user queue (4e).
 */
static void
sh_mem_init(struct atrium_amd_softc *sc, const struct gfx_gen *g)
{
	int i;

	for (i = 0; i < NUM_VMID; i++) {
		grbm_select(sc, g, 0, 0, 0, i);
		wr(sc, g->sh_mem_config, SH_MEM_CONFIG_DEFAULT);
		if (i >= FIRST_COMPUTE_VMID) {
			wr(sc, g->sh_mem_bases, SH_MEM_BASES_COMPUTE);
			wr(sc, g->spi_gdbg_vmid,
			    rd(sc, g->spi_gdbg_vmid) | TRAP_EN);
		} else if (i != 0)
			wr(sc, g->sh_mem_bases, SH_MEM_BASES_GFX);
	}
	grbm_select(sc, g, 0, 0, 0, 0);
	grbm_select(sc, g, 0, 0, 0, 1);
	device_printf(sc->dev, "gfx12: SH_MEM VMID 1: config %#x, bases %#x; ",
	    rd(sc, g->sh_mem_config), rd(sc, g->sh_mem_bases));
	grbm_select(sc, g, 0, 0, 0, FIRST_COMPUTE_VMID);
	printf("VMID %d: config %#x, bases %#x\n", FIRST_COMPUTE_VMID,
	    rd(sc, g->sh_mem_config), rd(sc, g->sh_mem_bases));
	grbm_select(sc, g, 0, 0, 0, 0);
}

static bool
cp_halted(struct atrium_amd_softc *sc, const struct gfx_gen *g)
{
	return ((rd(sc, g->cp_me_cntl) & CP_ME_HALTS) == CP_ME_HALTS &&
	    (rd(sc, g->cp_mec_rs64_cntl) & CP_MEC_HALT) != 0);
}

/*
 * Stage 4c. GC must be powered (the caller holds GfxOff off) and the hubs
 * set up (4a). Everything written here is undone by the mode 1 reset; the
 * CP is never unhalted.
 */
int
amd_gfx12_setup(struct atrium_amd_softc *sc)
{
	const struct gfx_gen *g = gfx_gen_find(sc);
	uint64_t start[3];
	uint32_t v, gb;
	int err;

	if (g == NULL) {
		device_printf(sc->dev, "gfx12: no table for GC %u.%u.%u; not "
		    "touching it\n", sc->ip.gc.major, sc->ip.gc.minor,
		    sc->ip.gc.rev);
		return (ENOTSUP);
	}
	v = rd(sc, g->rlc_bootload_status);
	if ((v & BOOTLOAD_COMPLETE) == 0) {
		device_printf(sc->dev, "gfx12: RLC autoload not complete "
		    "(%#x)\n", v);
		return (ENXIO);
	}
	gfx_report(sc, g, "before");
	/*
	 * Halt the CP ourselves before touching its start addresses
	 * (cp_gfx_enable(false), cp_compute_enable(false)). After the RLC
	 * autoload the halts are set, but a GfxOff cycle brings the block
	 * back with them clear (box, 2026-11-12 19:26: CP_ME_CNTL 0xa000,
	 * CP_MEC_RS64_CNTL 0x3c000000). If they do not stick: refuse.
	 */
	wr(sc, g->cp_me_cntl, rd(sc, g->cp_me_cntl) | CP_ME_HALTS);
	wr(sc, g->cp_mec_rs64_cntl, (rd(sc, g->cp_mec_rs64_cntl) &
	    ~CP_MEC_PIPE_ACTIVE) | CP_MEC_HALT | CP_MEC_PIPE_RESET |
	    CP_MEC_INVALIDATE_ICACHE);
	DELAY(50);
	if (!cp_halted(sc, g)) {
		gfx_report(sc, g, "halt refused");
		device_printf(sc->dev, "gfx12: CP would not halt; not "
		    "programming it\n");
		return (EIO);
	}
	gfx_report(sc, g, "halted");

	/* Golden registers. The rev-0 list only applies to PCI revision 0. */
	wr(sc, g->db_mem_config, rd(sc, g->db_mem_config) |
	    DB_MEM_CONFIG_GOLDEN);
	if (pci_get_revid(sc->dev) == 0)
		device_printf(sc->dev, "gfx12: PCI revision 0: rev-0 golden "
		    "settings not in the table yet\n");

	gb = rd(sc, g->gb_addr_config);
	/*
	 * Raw address-config fields, decoded as amdgpu's get_gb_addr_config
	 * (1 << field). They describe the address swizzle, not the shader
	 * topology: on the R9700 NUM_SHADER_ENGINES decodes to 16 while the
	 * part has 4 SEs x 2 SAs (the SA disable mask below leaves 8 SAs).
	 * The topology comes from the SA/RB masks (and discovery's gc_info).
	 */
	device_printf(sc->dev, "gfx12: GB_ADDR_CONFIG %#x (address fields: "
	    "pipes %u, se %u, rb/se %u, pkrs %u, interleave %u B)\n", gb,
	    1u << (gb & 7), 1u << ((gb >> 19) & 0xf), 1u << ((gb >> 26) & 3),
	    1u << ((gb >> 8) & 7), 1u << (8 + ((gb >> 3) & 7)));
	v = (rd(sc, g->cc_sa_disable) | rd(sc, g->user_sa_disable)) >> 8;
	device_printf(sc->dev, "gfx12: active SAs %d of 16 (mask %#x)\n",
	    16 - __builtin_popcount(v & 0xffff), ~v & 0xffff);
	device_printf(sc->dev, "gfx12: RB disable %#x/%#x, SA disable "
	    "%#x/%#x, DB_RING_CONTROL %#x\n", rd(sc, g->cc_rb_disable),
	    rd(sc, g->user_rb_disable), rd(sc, g->cc_sa_disable),
	    rd(sc, g->user_sa_disable), rd(sc, g->db_ring_control));

	err = rs64_starts(sc, start);
	if (err != 0)
		return (err);
	config_rs64(sc, g, start);

	/* gfxhub_v12_0_set_fault_enable_default: no CP halt on a fault. */
	wr(sc, g->cp_debug, rd(sc, g->cp_debug) | CP_DEBUG_UTCL1_HALT_DISABLE);

	/* constants_init */
	wr(sc, g->grbm_gfx_index, GRBM_GFX_INDEX_BROADCAST);
	wr(sc, g->grbm_cntl, (rd(sc, g->grbm_cntl) & ~GRBM_READ_TIMEOUT_MASK) |
	    0xff);
	sh_mem_init(sc, g);

	gfx_report(sc, g, "after");
	if (!cp_halted(sc, g)) {
		device_printf(sc->dev, "gfx12: stage 4c: CP NOT halted at "
		    "the end\n");
		return (EIO);
	}
	device_printf(sc->dev, "gfx12: stage 4c done; CP halted\n");
	return (0);
}
