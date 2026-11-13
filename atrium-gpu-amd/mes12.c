/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * mes12.c — stage 4d of the silicon bring-up (docs/spec/r9700-bare-metal.md
 * §4.4): start the MES (the GPU's own queue scheduler firmware, loaded by
 * the PSP in stage 3) and talk to it through its API ring.
 *
 * Interface: Linux's MIT-licensed mes_v12_0.c and amdgpu_mes.c, unified MES
 * (one image on both MES pipes, amdgpu's default):
 *   1. tell the RLC which queue is the KIQ (me 3, pipe 1);
 *   2. start both MES pipes at the image's start address;
 *   3. pipe 1's queue: MQD + HQD registers written directly;
 *   4. SET_HW_RESOURCES and SET_HW_RESOURCES_1 on pipe 1;
 *   5. pipe 0 (the scheduler) gets its queue by ADD_QUEUE (map_legacy_kq)
 *      sent to pipe 1; then SET_HW_RESOURCES(_1) on pipe 0, the aggregated
 *      doorbells, and QUERY_SCHEDULER_STATUS.
 * Every API call is followed by QUERY_SCHEDULER_STATUS carrying the ring's
 * fence (amdgpu's submit_pkt_and_poll_completion), and the driver polls that
 * fence in memory: nothing here needs an interrupt. With the IH ring up
 * (stage 4b), enable_mes_fence_int + TIME_STAMP_INT_ENABLE also make MES
 * raise an EOP interrupt (source 181, context 0xb1) per API, which is the
 * IH's first real source.
 *
 * Packet and MQD offsets are offsetof() over mes_v12_api_def.h and
 * v12_structs.h (computed on the host, listed by name below). Rings, MQDs,
 * EOP buffers and writeback live in system pages mapped into the GART.
 *
 * Stage 4e (kenv hw.atrium_gpu.dispatch=1) runs while MES is up: one
 * kernel compute queue (MEC1 pipe 0 queue 0) mapped by MES, the oracle's
 * write_42 dispatched through it by an indirect buffer, an end-of-pipe fence
 * with interrupt, and the output compared with the oracle's golden.
 *
 * Per version: mes_gens[] by GC version. Unknown: not touched (A3.10).
 */
#include "atrium_gpu_amd.h"

#include <sys/firmware.h>
#include <sys/rman.h>
#include <machine/bus.h>
#include <machine/atomic.h>
#include <dev/pci/pcireg.h>
#include <dev/pci/pcivar.h>
#include <vm/vm.h>
#include <vm/pmap.h>
#include <vm/vm_param.h>
#include <vm/vm_page.h>
#include <vm/vm_phys.h>

#include "write42_ib.h"	/* stage 4e: the oracle's write_42, generated */

#define S0(x)	(0u << 24 | (x))
#define S1(x)	(1u << 24 | (x))

struct mes_gen {
	uint8_t major, minor, rev_min, rev_max;
	const char *name;
	/* GC */
	uint32_t grbm_gfx_cntl, rlc_cp_schedulers, cp_mes_cntl;
	uint32_t mes_start, mes_start_hi, mes_gp3_lo, mes_instr_pntr;
	uint32_t hqd_vmid, hqd_doorbell, mqd_base, mqd_base_hi, mqd_control;
	uint32_t hqd_pq_base, hqd_pq_base_hi, hqd_rptr_report,
	    hqd_rptr_report_hi, hqd_pq_control, hqd_wptr_poll, hqd_wptr_poll_hi,
	    hqd_persistent, hqd_active, hqd_pq_rptr, hqd_pq_wptr_lo;
	uint32_t unmapped_doorbell, mes_db_cntl1, hqd_gfx_control, cpc_int_cntl;
	uint32_t hqd_dequeue_req, hqd_pq_wptr_hi, grbm_status, cp_stat, rlc_stat;
	uint32_t grbm_status2, grbm_status3, cp_cpc_status, cp_cpc_busy,
	    cp_cpc_busy2, cp_cpf_status, cp_busy_stat, rlc_gpm_stat;
	/* Firmware placement, per pipe (programmed by the PSP): read only. */
	uint32_t ic_base_lo, ic_base_hi, ic_base_cntl, md_base_lo, md_base_hi,
	    mi_bound_lo, mi_bound_hi, md_bound_lo, md_bound_hi, mscratch_lo,
	    mscratch_hi, local_base0_lo;
	/* CP engines and doorbell ranges (gfx_v12_0_cp_resume). */
	uint32_t cp_me_cntl, cp_mec_rs64_cntl, rb_db_lo, rb_db_hi, mec_db_lo,
	    mec_db_hi;
	uint32_t db_gfx_lo, db_gfx_hi, db_mec_lo, db_mec_hi;	/* values */
	/* NBIF segment 2 */
	uint32_t nbif_db_aper_en, nbif_db_entry0, nbif_db_entry3;
	/* Doorbell dword indices (amdgpu_doorbell.h NAVI10 MES_RING0/1 << 1). */
	uint32_t db_pipe0, db_pipe1, db_aggregated;
	/* Compute queue reset (mes_v12_0_reset_queue_mmio). */
	uint32_t spi_compute_queue_reset;
};

static const struct mes_gen mes_gens[] = {
	{ 12, 0, 0, 1, "MES 12 (GC 12.0.0/1, unified)",
	  S1(0x0900), S1(0x098a), S1(0x2807),
	  S1(0x2800), S1(0x289d), S1(0x2849), S1(0x2813),
	  S0(0x1fac), S0(0x1fb8), S0(0x1fa9), S0(0x1faa), S0(0x1fcb),
	  S0(0x1fb1), S0(0x1fb2), S0(0x1fb4), S0(0x1fb5), S0(0x1fba),
	  S0(0x1fb6), S0(0x1fb7), S0(0x1fad), S0(0x1fab), S0(0x1fb3),
	  S0(0x1fdf),
	  S1(0x0880), S1(0x283c), S0(0x1e9f), S0(0x1e54),
	  S0(0x1fc1), S0(0x1fe0), S0(0x0da4), S0(0x0f40), S1(0x4c04),
	  S0(0x0da2), S0(0x0da7), S0(0x0e24), S0(0x0e25), S0(0x0e2a),
	  S0(0x0e27), S0(0x0f3f), S1(0x4e6c),
	  S1(0x5850), S1(0x5851), S1(0x5852), S1(0x5854), S1(0x5855),
	  S1(0x585b), S1(0x585c), S1(0x585d), S1(0x585e), S1(0x2815),
	  S1(0x2814), S1(0x2883),
	  S1(0x0803), S1(0x2904), S0(0x1dfa), S0(0x1dfb), S0(0x1dfc),
	  S0(0x1dfd),
	  /* NAVI10 doorbell layout (soc24.c): gfx 0x8b..0xff, kiq 0..0x8a. */
	  (0x8b * 2) << 2, (0xff * 2) << 2, 0, (0x8a * 2) << 2,
	  0x00c0, 0x01cb, 0x01ce,
	  0x0b << 1, 0x0c << 1, 0x400,
	  S0(0x1f73) },
};

/* CP_MES_CNTL */
#define MES_INVALIDATE_ICACHE	0x00000010u
#define MES_PIPE0_RESET		0x00010000u
#define MES_PIPE1_RESET		0x00020000u
#define MES_PIPE0_ACTIVE	0x04000000u
#define MES_PIPE1_ACTIVE	0x08000000u
#define MES_HALT		0x40000000u
#define TIME_STAMP_INT_ENABLE	(1u << 26)

/* Layout of our GART block (bytes). */
#define MES_GART_VA		0x00020000ull	/* after the IH (4b) */
#define BLK_RING(p)		((p) == 1 ? 0x0000u : 0x1000u)
#define BLK_MQD(p)		((p) == 1 ? 0x2000u : 0x2800u)
#define BLK_EOP(p)		((p) == 1 ? 0x3000u : 0x3800u)
#define BLK_WB(p)		(0x4000u + (p) * 0x100u)
#define WB_RPTR			0x00
#define WB_WPTR			0x08
#define WB_FENCE		0x10
#define WB_STATUS		0x18
#define WB_SCH_CTX		0x20
#define WB_QUERY_FENCE		0x28
#define WB_SUSPEND_FENCE	0x40	/* pipe 0: SUSPEND's fence (kernel's) */
#define BLK_RES1(p)		(0x5000u + (p) * 0x1000u)
#define BLK_SIZE		0x7000u
/*
 * Per-pipe MSCRATCH (amdgpu's event-log layout without the log part):
 * zeroed and programmed into CP_MES_MSCRATCH before every unhalt, as
 * mainline mes_v12_0_enable does, so the firmware never walks stale
 * pointers or address 0 at activation (lead for the intermittent pipe
 * start, 2026-11-12). kenv hw.atrium_gpu.mes_mscratch=0 leaves it 0.
 */
#define MES_SCRATCH_VA		0x00100000ull
#define MES_SCRATCH_SIZE	0x40000u	/* AMDGPU_MES_MSCRATCH_SIZE */
#define RING_BYTES		4096u
#define RING_DW			(RING_BYTES / 4)
#define MES_EOP_SIZE		2048u

/* v12_compute_mqd (v12_structs.h), byte offsets; 2048 bytes. */
#define MQD_SIZE		2048u
#define MQD_HEADER		0
#define MQD_PIPELINESTAT	44
#define MQD_STM_SE0		92
#define MQD_STM_SE1		96
#define MQD_STM_SE2		104
#define MQD_STM_SE3		108
#define MQD_MISC_RESERVED	128
#define MQD_MQD_BASE_LO		512
#define MQD_MQD_BASE_HI		516
#define MQD_HQD_ACTIVE		520
#define MQD_HQD_VMID		524
#define MQD_PERSISTENT		528
#define MQD_QUANTUM		540
#define MQD_PQ_BASE_LO		544
#define MQD_PQ_BASE_HI		548
#define MQD_PQ_RPTR		552
#define MQD_RPTR_REPORT_LO	556
#define MQD_RPTR_REPORT_HI	560
#define MQD_WPTR_POLL_LO	564
#define MQD_WPTR_POLL_HI	568
#define MQD_DOORBELL		572
#define MQD_PQ_CONTROL		580
#define MQD_IB_CONTROL		596
#define MQD_IQ_TIMER		600
#define MQD_MQD_CONTROL		648
#define MQD_EOP_BASE_LO		660
#define MQD_EOP_BASE_HI		664
#define MQD_EOP_CONTROL		668
#define MQD_WPTR_LO		728
#define MQD_WPTR_HI		732
#define MQD_RESERVED_184	736

/* Register defaults (gc_11_0_0_default.h, as mes_v12_0.c uses them). */
#define EOP_CONTROL_DEFAULT	0x00000006u
#define MQD_CONTROL_DEFAULT	0x00000100u
#define PQ_CONTROL_DEFAULT	0x00308509u
#define PERSISTENT_DEFAULT	0x0be05501u
#define IB_CONTROL_DEFAULT	0x00300000u

/* MES API (mes_v12_api_def.h): 64-dword frames; byte offsets. */
#define API_DW			64
#define HDR_SET_HW_RSRC		0x00040001u	/* type 1, opcode 0, 64 dw */
#define HDR_SET_HW_RSRC_1	0x00040131u
#define HDR_QUERY_STATUS	0x000400b1u
#define HDR_ADD_QUEUE		0x00040021u
#define HDR_REMOVE_QUEUE	0x00040031u
#define HDR_SUSPEND		0x00040061u	/* opcode 6 */
#define HDR_RESUME		0x00040071u	/* opcode 7 */
#define HDR_RESET		0x00040081u	/* opcode 8 (MESAPI__RESET) */
#define RST_FLAGS		4	/* bit 0 reset_queue_only */
#define RST_GANG_CTX		8
#define RST_DOORBELL		16
#define RST_QUEUE_TYPE		32
#define RST_API_STATUS		112
#define HW_VMID_MMHUB		4
#define HW_VMID_GFXHUB		8
#define HW_COMPUTE_HQD		20	/* [8] */
#define HW_GFX_HQD		52	/* [2] */
#define HW_SDMA_HQD		60	/* [2] */
#define HW_AGG_DB		68	/* [5] */
#define HW_SCH_CTX		88
#define HW_QUERY_FENCE		96
#define HW_GC_BASE		104	/* [5] */
#define HW_MMHUB_BASE		136
#define HW_OSSSYS_BASE		168
#define HW_API_STATUS		200
#define HW_FLAGS		216	/* dw 54 */
#define HW_OVERSUB_TIMER	220
#define F_DISABLE_RESET		0x00000001u
#define F_DIFF_VMID_COMPUTE	0x00000002u
#define F_DISABLE_MES_LOG	0x00000004u
#define F_LEVEL_QUANTUM_CHECK	0x00000040u
#define F_REG_ACTIVE_POLL	0x00000400u
#define F_UNMAPPED_DB_BASICP	0x00100000u	/* handling mode 2 */
#define F_MES_FENCE_INT		0x00200000u
#define HW1_API_STATUS		8
#define HW1_KIQ_UNMAP_TIMEOUT	52
#define HW1_CLEANER_FENCE	64
#define QUERY_API_STATUS	8
#define AQ_PROCESS_ID		4
#define AQ_PTB			8
#define AQ_VA_START		16
#define AQ_VA_END		24
#define AQ_PROC_QUANTUM		32
#define AQ_PROC_CTX		40
#define AQ_GANG_QUANTUM		48
#define AQ_GANG_CTX		56
#define AQ_INPROC_PRIO		64
#define AQ_GANG_PRIO		68
#define AQ_DOORBELL		72
#define AQ_MQD_ADDR		80
#define AQ_WPTR_ADDR		88
#define AQ_QUEUE_TYPE		112
#define AQ_GDS_SIZE		120	/* queue size in dwords (amdgpu) */
#define AQ_VM_CNTX_CNTL		144
#define AQ_FLAGS		148	/* dw 37 */
#define AQ_MAP_LEGACY_KQ	0x00002000u
#define AQ_API_STATUS		152
#define AQ_PIPE_ID		200
#define AQ_QUEUE_ID		204
#define QUEUE_TYPE_SCHQ		3
#define RQ_DOORBELL		4
#define RQ_GANG_CTX		8
#define RQ_FLAGS		16	/* dw 4 */
#define RQ_UNMAP_LEGACY		0x00000008u
#define RQ_API_STATUS		24
#define RQ_PIPE_ID		40
#define RQ_QUEUE_ID		44
#define RQ_QUEUE_TYPE		60

struct mes_ring {
	int pipe;
	uint32_t db;		/* doorbell dword index */
	uint64_t wptr;		/* dwords */
	uint64_t seq;
};

struct mes {
	struct atrium_amd_softc *sc;
	const struct mes_gen *g;
	struct amd_gart *gt;
	struct resource *dbbar;
	int dbrid;
	bool udb_registered;	/* user-mode doorbell pages are fictitious */
	uint32_t suspend_seq;	/* the free rule's SUSPEND fence values */
	uint8_t *blk;		/* KVA of the GART block */
	vm_paddr_t blk_pa;
	uint64_t va;		/* GART address of the block */
	uint64_t uc_start;
	uint32_t version[2];
	uint32_t cpc_int_cntl[2];	/* as found, restored on teardown */
	bool sched_mapped;		/* pipe 0's queue mapped by MES */
	bool dispatch;			/* stage 4e: keep MEC1 queue 0 */
	uint16_t pci_cmd;		/* as found, restored on down */
	uint8_t *scratch;		/* 2 x MES_SCRATCH_SIZE, or NULL */
	vm_paddr_t scratch_pa;
	uint64_t scratch_va;
	struct mes_ring r[2];	/* index = pipe */
};

static bus_size_t
gcreg(struct atrium_amd_softc *sc, uint32_t r)
{
	return (((bus_size_t)sc->ip.gc.base[r >> 24] + (r & 0xffffff)) * 4);
}

static uint32_t
rd(struct mes *m, uint32_t r)
{
	return (amd_mmio_read32(m->sc, gcreg(m->sc, r)));
}

static void
wr(struct mes *m, uint32_t r, uint32_t v)
{
	amd_mmio_write32(m->sc, gcreg(m->sc, r), v);
}

static bus_size_t
nbreg(struct atrium_amd_softc *sc, uint32_t dw)
{
	return (((bus_size_t)sc->ip.nbif.base[2] + dw) * 4);
}

static void
grbm_select(struct mes *m, u_int me, u_int pipe)
{
	wr(m, m->g->grbm_gfx_cntl, (pipe & 3) | (me & 3) << 2);
}

static volatile uint32_t *
p32(struct mes *m, uint32_t off)
{
	return ((volatile uint32_t *)(m->blk + off));
}

static volatile uint64_t *
p64(struct mes *m, uint32_t off)
{
	return ((volatile uint64_t *)(m->blk + off));
}

static void
put32(uint8_t *b, uint32_t off, uint32_t v)
{
	memcpy(b + off, &v, 4);
}

static void
put64(uint8_t *b, uint32_t off, uint64_t v)
{
	memcpy(b + off, &v, 8);
}

static const struct mes_gen *
mes_gen_find(struct atrium_amd_softc *sc)
{
	const struct amd_ip_ver *gc = &sc->ip.gc;
	size_t i;

	for (i = 0; i < nitems(mes_gens); i++)
		if (mes_gens[i].major == gc->major &&
		    mes_gens[i].minor == gc->minor &&
		    gc->rev >= mes_gens[i].rev_min &&
		    gc->rev <= mes_gens[i].rev_max)
			return (&mes_gens[i]);
	return (NULL);
}

static void
mes_report(struct mes *m, const char *when)
{
	int p;

	device_printf(m->sc->dev, "mes: %s: CP_MES_CNTL %#x", when,
	    rd(m, m->g->cp_mes_cntl));
	for (p = 0; p < 2; p++) {
		grbm_select(m, 3, p);
		printf("; pipe %d: instr %#x, GP3 %#x, HQD active %u, rptr "
		    "%#x, wptr %#x", p, rd(m, m->g->mes_instr_pntr),
		    rd(m, m->g->mes_gp3_lo), rd(m, m->g->hqd_active),
		    rd(m, m->g->hqd_pq_rptr), rd(m, m->g->hqd_pq_wptr_lo));
	}
	grbm_select(m, 0, 0);
	printf("; GRBM_STATUS %#x, CP_STAT %#x, RLC_STAT %#x\n",
	    rd(m, m->g->grbm_status), rd(m, m->g->cp_stat),
	    rd(m, m->g->rlc_stat));
	device_printf(m->sc->dev, "mes: %s: GRBM_STATUS2 %#x, GRBM_STATUS3 "
	    "%#x, CP_CPC_STATUS %#x, CP_CPC_BUSY_STAT %#x/%#x, CP_CPF_STATUS "
	    "%#x, CP_BUSY_STAT %#x, RLC_GPM_STAT %#x\n", when,
	    rd(m, m->g->grbm_status2), rd(m, m->g->grbm_status3),
	    rd(m, m->g->cp_cpc_status), rd(m, m->g->cp_cpc_busy),
	    rd(m, m->g->cp_cpc_busy2), rd(m, m->g->cp_cpf_status),
	    rd(m, m->g->cp_busy_stat), rd(m, m->g->rlc_gpm_stat));
}

#define GRBM_BUSY_BITS	0xa8000000u	/* GUI_ACTIVE | CP_BUSY | ANY_ACTIVE */

/*
 * Box runs 84bd8b3f/7c536595: GRBM_STATUS took GUI_ACTIVE|CP_BUSY|
 * ANY_ACTIVE when MES answered and kept them after the halt, with CP_STAT
 * 0, and the card never idled. Watch whether they clear on their own.
 */
static bool
grbm_watch(struct mes *m, const char *when, int ms)
{
	uint32_t first = rd(m, m->g->grbm_status), v = first;
	int us;

	for (us = 0; us < ms * 1000; us += 1000) {
		v = rd(m, m->g->grbm_status);
		if ((v & GRBM_BUSY_BITS) == 0)
			break;
		DELAY(1000);
	}
	device_printf(m->sc->dev, "mes: %s: GRBM_STATUS %#x -> %#x, %s after "
	    "%d ms; CP_CPC_STATUS %#x, CP_CPF_STATUS %#x\n", when, first, v,
	    (v & GRBM_BUSY_BITS) == 0 ? "idle" : "still busy", us / 1000,
	    rd(m, m->g->cp_cpc_status), rd(m, m->g->cp_cpf_status));
	return ((v & GRBM_BUSY_BITS) == 0);
}

/*
 * Box runs from 20:36: MES pipes stopped starting (instr 0/1, GP3 0) and
 * the GFX hub reported UTCL2 faults (client 0xa src 0) at 0x3f01NN000 and
 * 0xfffff01NN000, the latter inside the MES data segment's address range.
 * Log where the PSP placed each pipe's code and data, read only.
 */
static void
mes_placement(struct mes *m, const char *when)
{
	const struct mes_gen *g = m->g;
	int p;

	for (p = 0; p < 2; p++) {
		grbm_select(m, 3, p);
		device_printf(m->sc->dev, "mes: %s: pipe %d IC_BASE %#x%08x "
		    "(cntl %#x), MIBOUND %#x%08x, MDBASE %#x%08x, MDBOUND "
		    "%#x%08x, MSCRATCH %#x%08x, LOCAL_BASE0 %#x\n", when, p,
		    rd(m, g->ic_base_hi), rd(m, g->ic_base_lo),
		    rd(m, g->ic_base_cntl), rd(m, g->mi_bound_hi),
		    rd(m, g->mi_bound_lo), rd(m, g->md_base_hi),
		    rd(m, g->md_base_lo), rd(m, g->md_bound_hi),
		    rd(m, g->md_bound_lo), rd(m, g->mscratch_hi),
		    rd(m, g->mscratch_lo), rd(m, g->local_base0_lo));
	}
	grbm_select(m, 0, 0);
}

#define ME_HALTS		0x14000000u	/* PFP_HALT | ME_HALT */
#define MEC_HALT		0x40000000u
#define MEC_ACTIVE		0x3c000000u	/* PIPE0..3_ACTIVE */
#define MEC_RESETS		0x000f0000u	/* PIPE0..3_RESET */
#define MEC_INV_ICACHE		0x00000010u

/*
 * gfx_v12_0_cp_resume, amdgpu_async_gfx_ring path (the default): doorbell
 * ranges, then MEC and PFP/ME un-halted, idle with no rings, BEFORE MES is
 * started. Box runs 6073c0fa..45267702 started MES with them halted (4c's
 * resting state): MES posts to the GFX pipe and the CPF stayed busy
 * forever (CP_CPF_STATUS 0x84000001), and the MES pipe start was flaky.
 * The RS64 start addresses were programmed in 4c.
 */
static int
cp_resume(struct mes *m)
{
	const struct mes_gen *g = m->g;
	uint32_t d;
	int us;

	wr(m, g->rb_db_lo, g->db_gfx_lo);
	wr(m, g->rb_db_hi, g->db_gfx_hi);
	wr(m, g->mec_db_lo, g->db_mec_lo);
	wr(m, g->mec_db_hi, g->db_mec_hi);
	/* gfx_v12_0_cp_compute_enable(true) */
	d = rd(m, g->cp_mec_rs64_cntl);
	d &= ~(MEC_INV_ICACHE | MEC_RESETS | MEC_HALT);
	d |= MEC_ACTIVE;
	wr(m, g->cp_mec_rs64_cntl, d);
	DELAY(50);
	/* gfx_v12_0_cp_gfx_enable(true): un-halt, wait for CP_STAT 0. */
	wr(m, g->cp_me_cntl, rd(m, g->cp_me_cntl) & ~ME_HALTS);
	for (us = 0; us < 100000 && rd(m, g->cp_stat) != 0; us++)
		DELAY(1);
	device_printf(m->sc->dev, "mes: CP running before MES (as amdgpu): "
	    "CP_ME_CNTL %#x, CP_MEC_RS64_CNTL %#x, CP_STAT %#x after %d us, "
	    "CP_CPF_STATUS %#x\n", rd(m, g->cp_me_cntl),
	    rd(m, g->cp_mec_rs64_cntl), rd(m, g->cp_stat), us,
	    rd(m, g->cp_cpf_status));
	return (us >= 100000 ? ETIMEDOUT : 0);
}

/* Back to 4c's resting state after MES: PFP/ME and MEC halted. */
static void
cp_halt(struct mes *m)
{
	const struct mes_gen *g = m->g;
	uint32_t d;
	int us;

	wr(m, g->cp_me_cntl, rd(m, g->cp_me_cntl) | ME_HALTS);
	for (us = 0; us < 100000 && rd(m, g->cp_stat) != 0; us++)
		DELAY(1);
	d = rd(m, g->cp_mec_rs64_cntl);
	d &= ~MEC_ACTIVE;
	d |= MEC_INV_ICACHE | MEC_RESETS | MEC_HALT;
	wr(m, g->cp_mec_rs64_cntl, d);
	DELAY(50);
	device_printf(m->sc->dev, "mes: CP halted again: CP_ME_CNTL %#x, "
	    "CP_MEC_RS64_CNTL %#x, CP_STAT %#x\n", rd(m, g->cp_me_cntl),
	    rd(m, g->cp_mec_rs64_cntl), rd(m, g->cp_stat));
}

/* mes_v12_0_mqd_init */
static void
mqd_init(struct mes *m, int pipe)
{
	struct mes_ring *r = &m->r[pipe];
	uint8_t *q = m->blk + BLK_MQD(pipe);
	uint64_t mqd = m->va + BLK_MQD(pipe), ring = m->va + BLK_RING(pipe);
	uint64_t eop = m->va + BLK_EOP(pipe), wb = m->va + BLK_WB(pipe);
	uint32_t t;

	memset(q, 0, MQD_SIZE);
	put32(q, MQD_HEADER, 0xC0310800u);
	put32(q, MQD_PIPELINESTAT, 1);
	put32(q, MQD_STM_SE0, ~0u);
	put32(q, MQD_STM_SE1, ~0u);
	put32(q, MQD_STM_SE2, ~0u);
	put32(q, MQD_STM_SE3, ~0u);
	put32(q, MQD_MISC_RESERVED, 7);
	put32(q, MQD_EOP_BASE_LO, (uint32_t)(eop >> 8));
	put32(q, MQD_EOP_BASE_HI, (uint32_t)(eop >> 40));
	/* EOP_SIZE: 2^(n+1) dwords = 2048 bytes -> n = 8. */
	put32(q, MQD_EOP_CONTROL, (EOP_CONTROL_DEFAULT & ~0x3fu) | 8);
	put32(q, MQD_PQ_RPTR, 0);
	put32(q, MQD_WPTR_LO, 0);
	put32(q, MQD_WPTR_HI, 0);
	put32(q, MQD_MQD_BASE_LO, (uint32_t)mqd & 0xfffffffcu);
	put32(q, MQD_MQD_BASE_HI, (uint32_t)(mqd >> 32));
	put32(q, MQD_MQD_CONTROL, MQD_CONTROL_DEFAULT);	/* VMID 0 */
	put32(q, MQD_PQ_BASE_LO, (uint32_t)(ring >> 8));
	put32(q, MQD_PQ_BASE_HI, (uint32_t)(ring >> 40));
	put32(q, MQD_RPTR_REPORT_LO, (uint32_t)(wb + WB_RPTR) & 0xfffffffcu);
	put32(q, MQD_RPTR_REPORT_HI, (uint32_t)((wb + WB_RPTR) >> 32) & 0xffff);
	put32(q, MQD_WPTR_POLL_LO, (uint32_t)(wb + WB_WPTR) & 0xfffffff8u);
	put32(q, MQD_WPTR_POLL_HI, (uint32_t)((wb + WB_WPTR) >> 32) & 0xffff);
	/*
	 * PQ_CONTROL as amdgpu computes it: QUEUE_SIZE 2^(9+1) dwords; its
	 * RPTR_BLOCK_SIZE expression lands outside the field, so 0; unordered
	 * dispatch, privileged, kernel queue, no rptr update.
	 */
	t = PQ_CONTROL_DEFAULT & ~(0x3fu | 0x3f00u);
	t |= (uint32_t)(ffs(RING_DW) - 2);
	t |= (1u << 27) | (1u << 28) | (1u << 30) | (1u << 31);
	t &= ~(1u << 29);
	put32(q, MQD_PQ_CONTROL, t);
	put32(q, MQD_DOORBELL, (r->db << 2) | (1u << 30));
	put32(q, MQD_HQD_VMID, 0);
	put32(q, MQD_HQD_ACTIVE, 1);
	put32(q, MQD_PERSISTENT, (PERSISTENT_DEFAULT & ~0x3ff00u) |
	    (0x55u << 8));
	put32(q, MQD_IB_CONTROL, IB_CONTROL_DEFAULT);
	put32(q, MQD_IQ_TIMER, 0);
	put32(q, MQD_QUANTUM, 0);
	put32(q, MQD_RESERVED_184, 1u << 15);	/* DB_UPDATED_MSG_EN */
	r->wptr = 0;
	*p64(m, BLK_WB(pipe) + WB_WPTR) = 0;
	*p64(m, BLK_WB(pipe) + WB_RPTR) = 0;
	memset(m->blk + BLK_RING(pipe), 0, RING_BYTES);
	memset(m->blk + BLK_EOP(pipe), 0, MES_EOP_SIZE);
}

/* mes_v12_0_queue_init_register: the KIQ pipe's HQD by register. */
static void
queue_init_register(struct mes *m, int pipe)
{
	const struct mes_gen *g = m->g;
	uint8_t *q = m->blk + BLK_MQD(pipe);
	uint32_t v;

#define MQ(o)	({ uint32_t _v; memcpy(&_v, q + (o), 4); _v; })
	grbm_select(m, 3, pipe);
	wr(m, g->hqd_vmid, rd(m, g->hqd_vmid) & ~0xfu);
	wr(m, g->hqd_doorbell, rd(m, g->hqd_doorbell) & ~(1u << 30));
	wr(m, g->mqd_base, MQ(MQD_MQD_BASE_LO));
	wr(m, g->mqd_base_hi, MQ(MQD_MQD_BASE_HI));
	wr(m, g->mqd_control, 0);
	wr(m, g->hqd_pq_base, MQ(MQD_PQ_BASE_LO));
	wr(m, g->hqd_pq_base_hi, MQ(MQD_PQ_BASE_HI));
	wr(m, g->hqd_rptr_report, MQ(MQD_RPTR_REPORT_LO));
	wr(m, g->hqd_rptr_report_hi, MQ(MQD_RPTR_REPORT_HI));
	wr(m, g->hqd_pq_control, MQ(MQD_PQ_CONTROL));
	wr(m, g->hqd_wptr_poll, MQ(MQD_WPTR_POLL_LO));
	wr(m, g->hqd_wptr_poll_hi, MQ(MQD_WPTR_POLL_HI));
	wr(m, g->hqd_doorbell, MQ(MQD_DOORBELL));
	wr(m, g->hqd_persistent, MQ(MQD_PERSISTENT));
	wr(m, g->hqd_active, MQ(MQD_HQD_ACTIVE));
	v = rd(m, g->hqd_active);
	grbm_select(m, 0, 0);
#undef MQ
	device_printf(m->sc->dev, "mes: pipe %d HQD by register: active %u, "
	    "doorbell %#x\n", pipe, v, m->r[pipe].db);
}

/* mes_v12_0_enable */
static void
mes_enable(struct mes *m, bool on)
{
	const struct mes_gen *g = m->g;
	uint32_t d;
	int p;

	if (!on) {
		d = rd(m, g->cp_mes_cntl);
		d &= ~(MES_PIPE0_ACTIVE | MES_PIPE1_ACTIVE);
		d |= MES_INVALIDATE_ICACHE | MES_PIPE0_RESET |
		    MES_PIPE1_RESET | MES_HALT;
		wr(m, g->cp_mes_cntl, d);
		return;
	}
	if (m->scratch != NULL)
		memset(m->scratch, 0, 2 * MES_SCRATCH_SIZE);
	for (p = 0; p < 2; p++) {
		grbm_select(m, 3, p);
		if (m->scratch != NULL) {
			uint64_t va = m->scratch_va + p * MES_SCRATCH_SIZE;

			wr(m, g->mscratch_lo, (uint32_t)va);
			wr(m, g->mscratch_hi, (uint32_t)(va >> 32));
		}
		d = rd(m, g->cp_mes_cntl);
		wr(m, g->cp_mes_cntl, d | (p == 0 ? MES_PIPE0_RESET :
		    MES_PIPE1_RESET));
		wr(m, g->mes_start, (uint32_t)(m->uc_start >> 2));
		wr(m, g->mes_start_hi, (uint32_t)(m->uc_start >> 34));
		/* Unhalt, activate one pipe each loop. */
		wr(m, g->cp_mes_cntl, MES_PIPE0_ACTIVE |
		    (p != 0 ? MES_PIPE1_ACTIVE : 0));
	}
	grbm_select(m, 0, 0);
	DELAY(500);
}

/* Write `n` dwords at the ring's wptr. */
static void
ring_write(struct mes *m, int pipe, const uint32_t *d, u_int n)
{
	struct mes_ring *r = &m->r[pipe];
	volatile uint32_t *ring = p32(m, BLK_RING(pipe));
	u_int i;

	for (i = 0; i < n; i++)
		ring[(r->wptr + i) & (RING_DW - 1)] = d[i];
	r->wptr += n;
}

/* amdgpu_ring_commit + mes_v12_0_ring_set_wptr: writeback, then doorbell. */
static void
ring_commit(struct mes *m, int pipe)
{
	struct mes_ring *r = &m->r[pipe];

	*p64(m, BLK_WB(pipe) + WB_WPTR) = r->wptr;
	wmb();
	bus_write_8(m->dbbar, (bus_size_t)r->db * 4, r->wptr);
}

/*
 * mes_v12_0_submit_pkt_and_poll_completion: the packet with its own status
 * fence, then QUERY_SCHEDULER_STATUS carrying the ring's sequence number;
 * poll the ring fence, then the status (low 32 bits 0 = failed, high 32
 * bits say why).
 */
static int
mes_api(struct mes *m, int pipe, uint32_t *pkt, uint32_t status_off,
    const char *what)
{
	struct mes_ring *r = &m->r[pipe];
	uint32_t q[API_DW];
	uint64_t status_va = m->va + BLK_WB(pipe) + WB_STATUS;
	uint64_t fence_va = m->va + BLK_WB(pipe) + WB_FENCE;
	uint64_t st;
	int us;

	*p64(m, BLK_WB(pipe) + WB_STATUS) = 0;
	put64((uint8_t *)pkt, status_off, status_va);
	put64((uint8_t *)pkt, status_off + 8, 1);
	r->seq++;
	memset(q, 0, sizeof(q));
	q[0] = HDR_QUERY_STATUS;
	put64((uint8_t *)q, QUERY_API_STATUS, fence_va);
	put64((uint8_t *)q, QUERY_API_STATUS + 8, r->seq);
	ring_write(m, pipe, pkt, API_DW);
	ring_write(m, pipe, q, API_DW);
	ring_commit(m, pipe);
	for (us = 0; us < 2100000; us += 10) {
		if ((uint32_t)*p64(m, BLK_WB(pipe) + WB_FENCE) >=
		    (uint32_t)r->seq)
			break;
		DELAY(10);
	}
	st = *p64(m, BLK_WB(pipe) + WB_STATUS);
	if (us >= 2100000 || (uint32_t)st == 0) {
		device_printf(m->sc->dev, "mes: pipe %d %s: %s (fence %#jx of "
		    "%#jx, status %#jx, rptr %#jx)\n", pipe, what,
		    us >= 2100000 ? "no answer in 2.1 s" : "refused",
		    (uintmax_t)*p64(m, BLK_WB(pipe) + WB_FENCE),
		    (uintmax_t)r->seq, (uintmax_t)st,
		    (uintmax_t)*p64(m, BLK_WB(pipe) + WB_RPTR));
		return (us >= 2100000 ? ETIMEDOUT : EIO);
	}
	device_printf(m->sc->dev, "mes: pipe %d %s: ok in %d us (status "
	    "%#jx)\n", pipe, what, us, (uintmax_t)st);
	return (0);
}

/* mes_v12_0_set_hw_resources */
static int
set_hw_resources(struct mes *m, int pipe)
{
	struct atrium_amd_softc *sc = m->sc;
	uint32_t pkt[API_DW];
	uint8_t *b = (uint8_t *)pkt;
	uint32_t ver = m->version[pipe] & 0xfff;
	int i;

	memset(pkt, 0, sizeof(pkt));
	pkt[0] = HDR_SET_HW_RSRC;
	if (pipe == 0) {
		/* VMIDs 8-15 for user queues, as amdgpu's uq masks. */
		put32(b, HW_VMID_MMHUB, 0xff00);
		put32(b, HW_VMID_GFXHUB, 0xff00);
		/*
		 * MEC1 pipes 0-1, 4 queues each. Stage 4e keeps pipe 0 queue
		 * 0 for the kernel compute queue, as amdgpu keeps its KCQs.
		 */
		put32(b, HW_COMPUTE_HQD + 0, m->dispatch ? 0xe : 0xf);
		put32(b, HW_COMPUTE_HQD + 4, 0xf);
		/*
		 * gfx pipe 0, queue 0 kept, as amdgpu. (Box run 45267702: with
		 * 0 the CPF stuck busy just the same, so the mask was not the
		 * cause; PFP/ME being halted under MES was. See cp_resume().)
		 */
		put32(b, HW_GFX_HQD, 0xfe);
		put32(b, HW_SDMA_HQD + 0, 0xfc);
		put32(b, HW_SDMA_HQD + 4, 0xfc);
		for (i = 0; i < 5; i++)
			put32(b, HW_AGG_DB + 4 * i, m->g->db_aggregated + 2 * i);
	}
	put64(b, HW_SCH_CTX, m->va + BLK_WB(pipe) + WB_SCH_CTX);
	put64(b, HW_QUERY_FENCE, m->va + BLK_WB(pipe) + WB_QUERY_FENCE);
	for (i = 0; i < 5 && i < AMD_IP_NBASE; i++) {
		put32(b, HW_GC_BASE + 4 * i, sc->ip.gc.base[i]);
		put32(b, HW_MMHUB_BASE + 4 * i, sc->ip.mmhub.base[i]);
		put32(b, HW_OSSSYS_BASE + 4 * i, sc->ip.osssys.base[i]);
	}
	put32(b, HW_FLAGS, F_DISABLE_RESET | F_DISABLE_MES_LOG |
	    F_DIFF_VMID_COMPUTE | F_REG_ACTIVE_POLL | F_LEVEL_QUANTUM_CHECK |
	    F_UNMAPPED_DB_BASICP | F_MES_FENCE_INT);
	put32(b, HW_OVERSUB_TIMER, ver < 0x8b ? 0 : 50);
	return (mes_api(m, pipe, pkt, HW_API_STATUS, "SET_HW_RESOURCES"));
}

static int
set_hw_resources_1(struct mes *m, int pipe)
{
	uint32_t pkt[API_DW];

	memset(pkt, 0, sizeof(pkt));
	pkt[0] = HDR_SET_HW_RSRC_1;
	put32((uint8_t *)pkt, HW1_KIQ_UNMAP_TIMEOUT, 0xa);
	put64((uint8_t *)pkt, HW1_CLEANER_FENCE, m->va + BLK_RES1(pipe));
	return (mes_api(m, pipe, pkt, HW1_API_STATUS, "SET_HW_RESOURCES_1"));
}

static int
query_status(struct mes *m, int pipe)
{
	uint32_t pkt[API_DW];

	memset(pkt, 0, sizeof(pkt));
	pkt[0] = HDR_QUERY_STATUS;
	return (mes_api(m, pipe, pkt, QUERY_API_STATUS,
	    "QUERY_SCHEDULER_STATUS"));
}

/* mes_v12_0_map_legacy_queue for the scheduler ring: sent to pipe 1. */
static int
map_sched_queue(struct mes *m)
{
	uint32_t pkt[API_DW];
	uint8_t *b = (uint8_t *)pkt;

	memset(pkt, 0, sizeof(pkt));
	pkt[0] = HDR_ADD_QUEUE;
	put32(b, AQ_PIPE_ID, 0);
	put32(b, AQ_QUEUE_ID, 0);
	put32(b, AQ_DOORBELL, m->r[0].db);
	put64(b, AQ_MQD_ADDR, m->va + BLK_MQD(0));
	put64(b, AQ_WPTR_ADDR, m->va + BLK_WB(0) + WB_WPTR);
	put32(b, AQ_QUEUE_TYPE, QUEUE_TYPE_SCHQ);
	put32(b, AQ_FLAGS, AQ_MAP_LEGACY_KQ);
	return (mes_api(m, 1, pkt, AQ_API_STATUS, "ADD_QUEUE (scheduler)"));
}

/* mes_v12_0_unmap_legacy_queue for the scheduler ring (uni: to pipe 1). */
static int
unmap_sched_queue(struct mes *m)
{
	uint32_t pkt[API_DW];
	uint8_t *b = (uint8_t *)pkt;

	memset(pkt, 0, sizeof(pkt));
	pkt[0] = HDR_REMOVE_QUEUE;
	put32(b, RQ_DOORBELL, m->r[0].db);
	put32(b, RQ_FLAGS, RQ_UNMAP_LEGACY);
	put32(b, RQ_PIPE_ID, 0);
	put32(b, RQ_QUEUE_ID, 0);
	put32(b, RQ_QUEUE_TYPE, QUEUE_TYPE_SCHQ);
	return (mes_api(m, 1, pkt, RQ_API_STATUS, "REMOVE_QUEUE (scheduler)"));
}

/*
 * mes_v12_0_kiq_dequeue_sched, for any MES pipe whose HQD is still active:
 * dequeue request, wait for inactive, doorbell off, pointers zeroed.
 */
static bool
hqd_dequeue_me(struct mes *m, u_int me, int pipe)
{
	const struct mes_gen *g = m->g;
	uint32_t d;
	int us = 0;

	grbm_select(m, me, pipe);
	if (rd(m, g->hqd_active) & 1) {
		wr(m, g->hqd_dequeue_req, 1);
		for (; us < 100000 && (rd(m, g->hqd_active) & 1); us += 10)
			DELAY(10);
		wr(m, g->hqd_dequeue_req, 0);
	}
	d = rd(m, g->hqd_doorbell);
	d &= ~(1u << 30);		/* DOORBELL_EN */
	d |= 1u << 31;			/* DOORBELL_HIT: clear */
	wr(m, g->hqd_doorbell, d);
	wr(m, g->hqd_doorbell, 0);
	wr(m, g->hqd_pq_wptr_lo, 0);
	wr(m, g->hqd_pq_wptr_hi, 0);
	wr(m, g->hqd_pq_rptr, 0);
	d = rd(m, g->hqd_active);
	grbm_select(m, 0, 0);
	device_printf(m->sc->dev, "mes: me %u pipe %d HQD dequeued: active "
	    "%u%s\n", me, pipe, d & 1, us >= 100000 ? " (dequeue timed out)" :
	    "");
	return ((d & 1) == 0);
}

static void
hqd_dequeue(struct mes *m, int pipe)
{
	(void)hqd_dequeue_me(m, 3, pipe);
}

/*
 * Undo 4d so the card can idle (kiq_hw_fini + mes_enable(false)): the
 * scheduler queue unmapped by MES, both MES HQDs dequeued, unmapped
 * doorbell handling off, interrupt enables restored, then MES halted.
 */
static void
mes_teardown(struct mes *m)
{
	const struct mes_gen *g = m->g;
	int p;

	if (m->sched_mapped)
		(void)unmap_sched_queue(m);
	hqd_dequeue(m, 0);
	hqd_dequeue(m, 1);
	wr(m, g->unmapped_doorbell, rd(m, g->unmapped_doorbell) & ~1u);
	for (p = 0; p < 2; p++) {
		grbm_select(m, 3, p);
		wr(m, g->cpc_int_cntl, m->cpc_int_cntl[p]);
	}
	grbm_select(m, 0, 0);
	(void)grbm_watch(m, "queues gone, MES still running", 500);
	mes_enable(m, false);
	DELAY(100);
	(void)grbm_watch(m, "MES halted, CP still running", 500);
	cp_halt(m);
}

/*
 * Stage 4e: the first dispatch. One kernel compute queue, MEC1 (me 1) pipe
 * 0 queue 0, VMID 0, built as gfx_v12_0_compute_mqd_init does and mapped by
 * MES (mes_v12_0_map_legacy_queue: ADD_QUEUE with map_legacy_kq, sent to
 * the scheduler pipe 0 in unified mode). Its ring carries one
 * INDIRECT_BUFFER (gfx_v12_0_ring_emit_ib_compute) pointing at the oracle's
 * write_42 IB, then RELEASE_MEM with an interrupt (gfx_v12_0_ring_emit_fence).
 * Everything lives in one block of system pages in the GART.
 */
#define DISP_GART_VA		0x00040000ull	/* after MES's block */
#define DISP_RING		0x0000u
#define DISP_MQD		0x1000u
#define DISP_EOP		0x1800u		/* GFX12_MEC_HPD_SIZE */
#define DISP_WB			0x2000u		/* rptr 0, wptr 8, fence 0x10 */
#define DISP_CODE		0x3000u		/* shader; IB at +W42_IB_OFFSET */
#define DISP_OUT		0x5000u
#define DISP_SIZE		0x6000u
#define DISP_EOP_SIZE		2048u
#define KCQ_DOORBELL		(0x03u << 1)	/* NAVI10 MEC_RING0 << 1 */
#define QUEUE_TYPE_COMPUTE	1
#define DISP_SEQ		0x4e000001u
#define DISP_POISON		0xdeadbeefu
#define DISP_TIMEOUT_US		1000000

#define PKT3(op, n)		(3u << 30 | ((n) & 0x3fffu) << 16 | (op) << 8)
#define IT_INDIRECT_BUFFER	0x3fu
#define IB_VALID		(1u << 23)
#define RM_EVENT_TYPE(x)	(x)
#define RM_EVENT_INDEX(x)	((x) << 8)
#define RM_GCR_GL2_WB		(1u << 21)
#define RM_GCR_SEQ		(1u << 22)
#define RM_CACHE_POLICY(x)	((x) << 25)
#define RM_DATA_SEL(x)		((x) << 29)
#define RM_INT_SEL(x)		((x) << 24)
#define CACHE_FLUSH_AND_INV_TS_EVENT 0x14u

struct disp {
	uint8_t *blk;
	vm_paddr_t pa;
	uint64_t va;
	uint32_t int_cntl;	/* me 1 pipe 0 CPC_INT_CNTL as found */
};

/* gfx_v12_0_compute_mqd_init for a kernel queue, not active (MES maps it). */
static void
kcq_mqd_init(struct disp *d)
{
	uint8_t *q = d->blk + DISP_MQD;
	uint64_t mqd = d->va + DISP_MQD, ring = d->va + DISP_RING;
	uint64_t eop = d->va + DISP_EOP, wb = d->va + DISP_WB;
	uint32_t t;

	memset(q, 0, MQD_SIZE);
	put32(q, MQD_HEADER, 0xC0310800u);
	put32(q, MQD_PIPELINESTAT, 1);
	put32(q, MQD_STM_SE0, ~0u);
	put32(q, MQD_STM_SE1, ~0u);
	put32(q, MQD_STM_SE2, ~0u);
	put32(q, MQD_STM_SE3, ~0u);
	put32(q, MQD_MISC_RESERVED, 7);
	put32(q, MQD_EOP_BASE_LO, (uint32_t)(eop >> 8));
	put32(q, MQD_EOP_BASE_HI, (uint32_t)(eop >> 40));
	/* EOP_SIZE: 2^(n+1) dwords; 2048 bytes = 512 dwords -> 8. */
	put32(q, MQD_EOP_CONTROL, (EOP_CONTROL_DEFAULT & ~0x3fu) | 8);
	put32(q, MQD_MQD_BASE_LO, (uint32_t)mqd & 0xfffffffcu);
	put32(q, MQD_MQD_BASE_HI, (uint32_t)(mqd >> 32));
	put32(q, MQD_MQD_CONTROL, MQD_CONTROL_DEFAULT);	/* VMID 0 */
	put32(q, MQD_PQ_BASE_LO, (uint32_t)(ring >> 8));
	put32(q, MQD_PQ_BASE_HI, (uint32_t)(ring >> 40));
	/*
	 * QUEUE_SIZE 2^(9+1) dwords, RPTR_BLOCK_SIZE log2(4096 / 4) - 1,
	 * UNORD_DISPATCH, PRIV_STATE, KMD_QUEUE.
	 */
	t = PQ_CONTROL_DEFAULT & ~(0x3fu | 0x3f00u);
	t |= (uint32_t)(ffs(RING_DW) - 2) | (9u << 8);
	t |= (1u << 28) | (1u << 30) | (1u << 31);
	put32(q, MQD_PQ_CONTROL, t);
	put32(q, MQD_RPTR_REPORT_LO, (uint32_t)wb & 0xfffffffcu);
	put32(q, MQD_RPTR_REPORT_HI, (uint32_t)(wb >> 32) & 0xffff);
	put32(q, MQD_WPTR_POLL_LO, (uint32_t)(wb + 8) & 0xfffffffcu);
	put32(q, MQD_WPTR_POLL_HI, (uint32_t)((wb + 8) >> 32) & 0xffff);
	put32(q, MQD_DOORBELL, (KCQ_DOORBELL << 2) | (1u << 30));
	put32(q, MQD_PQ_RPTR, 0);
	put32(q, MQD_HQD_VMID, 0);
	put32(q, MQD_PERSISTENT, (PERSISTENT_DEFAULT & ~0x3ff00u) |
	    (0x55u << 8));
	put32(q, MQD_IB_CONTROL, IB_CONTROL_DEFAULT);	/* MIN_IB_AVAIL 3 */
	put32(q, MQD_QUANTUM, 0x111u);	/* EN, SCALE 1, DURATION 1 */
	put32(q, MQD_HQD_ACTIVE, 0);
}

static int
kcq_map(struct mes *m, struct disp *d)
{
	uint32_t pkt[API_DW];
	uint8_t *b = (uint8_t *)pkt;

	memset(pkt, 0, sizeof(pkt));
	pkt[0] = HDR_ADD_QUEUE;
	put32(b, AQ_PIPE_ID, 0);
	put32(b, AQ_QUEUE_ID, 0);
	put32(b, AQ_DOORBELL, KCQ_DOORBELL);
	put64(b, AQ_MQD_ADDR, d->va + DISP_MQD);
	put64(b, AQ_WPTR_ADDR, d->va + DISP_WB + 8);
	put32(b, AQ_QUEUE_TYPE, QUEUE_TYPE_COMPUTE);
	put32(b, AQ_FLAGS, AQ_MAP_LEGACY_KQ);
	return (mes_api(m, 0, pkt, AQ_API_STATUS, "ADD_QUEUE (compute)"));
}

static int
kcq_unmap(struct mes *m)
{
	uint32_t pkt[API_DW];
	uint8_t *b = (uint8_t *)pkt;

	memset(pkt, 0, sizeof(pkt));
	pkt[0] = HDR_REMOVE_QUEUE;
	put32(b, RQ_DOORBELL, KCQ_DOORBELL);
	put32(b, RQ_FLAGS, RQ_UNMAP_LEGACY);
	put32(b, RQ_PIPE_ID, 0);
	put32(b, RQ_QUEUE_ID, 0);
	put32(b, RQ_QUEUE_TYPE, QUEUE_TYPE_COMPUTE);
	return (mes_api(m, 0, pkt, RQ_API_STATUS, "REMOVE_QUEUE (compute)"));
}

static void
kcq_report(struct mes *m, struct disp *d, const char *when)
{
	const struct mes_gen *g = m->g;
	uint32_t act, rptr, wlo, db;

	grbm_select(m, 1, 0);
	act = rd(m, g->hqd_active);
	rptr = rd(m, g->hqd_pq_rptr);
	wlo = rd(m, g->hqd_pq_wptr_lo);
	db = rd(m, g->hqd_doorbell);
	grbm_select(m, 0, 0);
	device_printf(m->sc->dev, "dispatch: %s: HQD active %u, rptr %#x, "
	    "wptr %#x, doorbell %#x; rptr report %#x, fence %#x; CP_STAT %#x, "
	    "CPC_STATUS %#x, GRBM_STATUS %#x\n", when, act & 1, rptr, wlo, db,
	    *(volatile uint32_t *)(d->blk + DISP_WB),
	    *(volatile uint32_t *)(d->blk + DISP_WB + 0x10), rd(m, g->cp_stat),
	    rd(m, g->cp_cpc_status), rd(m, g->grbm_status));
}

/*
 * Map the queue, ring the doorbell, wait for the fence, compare the output,
 * unmap. Returns 0 only when all 64 words are the golden 42. The block is
 * freed only once the queue is known to be gone; otherwise it is leaked so
 * the GPU can never write freed memory.
 */
static int
dispatch_run(struct mes *m)
{
	struct atrium_amd_softc *sc = m->sc;
	const struct mes_gen *g = m->g;
	struct amd_gart *gt = m->gt;
	struct disp d;
	volatile uint32_t *out, *ring;
	uint64_t ib, fence;
	uint32_t i, bad = 0, first_bad = 0, n = 0;
	int err, us;
	bool mapped = false, gone = true;

	memset(&d, 0, sizeof(d));
	d.va = gt->gart_start + DISP_GART_VA;
	if (d.va + DISP_CODE != W42_CODE_VA || d.va + DISP_OUT != W42_DATA_VA ||
	    d.va + DISP_SIZE > gt->gart_end + 1) {
		device_printf(sc->dev, "dispatch: write_42 built for code %#jx "
		    "out %#jx, block at %#jx; regenerate write42_ib.h\n",
		    (uintmax_t)W42_CODE_VA, (uintmax_t)W42_DATA_VA,
		    (uintmax_t)d.va);
		return (EINVAL);
	}
	d.blk = contigmalloc(DISP_SIZE, M_DEVBUF, M_WAITOK | M_ZERO, 0,
	    ~(vm_paddr_t)0, PAGE_SIZE, 0);
	d.pa = vtophys(d.blk);
	for (i = 0; i < DISP_SIZE / PAGE_SIZE; i++)
		(void)amd_gmc12_gart_map(gt, d.va + i * PAGE_SIZE,
		    d.pa + i * PAGE_SIZE);
	if ((err = amd_gmc12_flush(gt)) != 0)
		goto out_unmap;

	memcpy(d.blk + DISP_CODE, w42_shader, sizeof(w42_shader));
	memcpy(d.blk + DISP_CODE + W42_IB_OFFSET, w42_ib, sizeof(w42_ib));
	out = (volatile uint32_t *)(d.blk + DISP_OUT);
	for (i = 0; i < W42_OUT_DW; i++)
		out[i] = DISP_POISON;
	kcq_mqd_init(&d);

	/* gfx_v12_0_set_compute_eop_interrupt_state(me 1, pipe 0, on) */
	grbm_select(m, 1, 0);
	d.int_cntl = rd(m, g->cpc_int_cntl);
	wr(m, g->cpc_int_cntl, d.int_cntl | TIME_STAMP_INT_ENABLE);
	grbm_select(m, 0, 0);

	device_printf(sc->dev, "dispatch: block at GART %#jx (pa %#jx), "
	    "shader %#jx, IB %#jx (%u dw), out %#jx, doorbell %#x\n",
	    (uintmax_t)d.va, (uintmax_t)d.pa, (uintmax_t)W42_CODE_VA,
	    (uintmax_t)(W42_CODE_VA + W42_IB_OFFSET), W42_IB_DW,
	    (uintmax_t)W42_DATA_VA, KCQ_DOORBELL);
	if ((err = kcq_map(m, &d)) != 0) {
		kcq_report(m, &d, "ADD_QUEUE failed");
		goto out_queue;
	}
	mapped = true;
	kcq_report(m, &d, "mapped");

	ring = (volatile uint32_t *)(d.blk + DISP_RING);
	ib = d.va + DISP_CODE + W42_IB_OFFSET;
	fence = d.va + DISP_WB + 0x10;
	ring[n++] = PKT3(IT_INDIRECT_BUFFER, 2);
	ring[n++] = (uint32_t)ib;
	ring[n++] = (uint32_t)(ib >> 32);
	ring[n++] = IB_VALID | W42_IB_DW;		/* VMID 0 */
	ring[n++] = PKT3(IT_RELEASE_MEM, 6);
	ring[n++] = RM_GCR_SEQ | RM_GCR_GL2_WB | RM_CACHE_POLICY(3) |
	    RM_EVENT_TYPE(CACHE_FLUSH_AND_INV_TS_EVENT) | RM_EVENT_INDEX(5);
	ring[n++] = RM_DATA_SEL(1) | RM_INT_SEL(2);	/* 32 bit, interrupt */
	ring[n++] = (uint32_t)fence;
	ring[n++] = (uint32_t)(fence >> 32);
	ring[n++] = DISP_SEQ;
	ring[n++] = 0;
	ring[n++] = 0;
	*(volatile uint64_t *)(d.blk + DISP_WB + 8) = n;
	wmb();
	bus_write_8(m->dbbar, (bus_size_t)KCQ_DOORBELL * 4, n);

	for (us = 0; us < DISP_TIMEOUT_US; us += 10) {
		if (*(volatile uint32_t *)(d.blk + DISP_WB + 0x10) == DISP_SEQ)
			break;
		DELAY(10);
	}
	if (us >= DISP_TIMEOUT_US) {
		device_printf(sc->dev, "dispatch: no fence in %d ms\n",
		    DISP_TIMEOUT_US / 1000);
		kcq_report(m, &d, "fence timeout");
		mes_report(m, "fence timeout");
		amd_gmc12_fault_report(gt, "fence timeout");
		err = ETIMEDOUT;
	} else {
		device_printf(sc->dev, "dispatch: fence %#x after %d us\n",
		    DISP_SEQ, us);
		kcq_report(m, &d, "fence");
	}
	for (i = 0; i < W42_OUT_DW; i++)
		if (out[i] != W42_OUT_VALUE && bad++ == 0)
			first_bad = i;
	if (bad != 0)
		device_printf(sc->dev, "dispatch: out: %u of %u words wrong, "
		    "first out[%u] = %#x (want %#x)\n", bad, W42_OUT_DW,
		    first_bad, out[first_bad], W42_OUT_VALUE);
	else
		device_printf(sc->dev, "dispatch: out: all %u words %#x, "
		    "matches the oracle golden\n", W42_OUT_DW, W42_OUT_VALUE);
	if (err == 0 && bad != 0)
		err = EIO;
	amd_gmc12_fault_report(gt, "after dispatch");

out_queue:
	if (mapped && kcq_unmap(m) != 0)
		kcq_report(m, &d, "REMOVE_QUEUE failed");
	gone = hqd_dequeue_me(m, 1, 0);
	grbm_select(m, 1, 0);
	wr(m, g->cpc_int_cntl, d.int_cntl);
	grbm_select(m, 0, 0);
out_unmap:
	for (i = 0; i < DISP_SIZE / PAGE_SIZE; i++) {
		uint64_t idx = (d.va - gt->gart_start) / PAGE_SIZE + i;

		bus_write_4(gt->vram, gt->table_off + idx * 8, 0);
		bus_write_4(gt->vram, gt->table_off + idx * 8 + 4, 0);
	}
	(void)amd_gmc12_flush(gt);
	if (gone)
		contigfree(d.blk, DISP_SIZE, M_DEVBUF);
	else
		device_printf(sc->dev, "dispatch: queue still active; its "
		    "%u KiB block is leaked, not freed\n", DISP_SIZE >> 10);
	device_printf(sc->dev, "dispatch: stage 4e: %s\n", err == 0 ?
	    "write_42 PASS (matches the oracle)" : "FAILED");
	return (err);
}

/*
 * Stage 5b: a per-client page table on silicon. VMID 1 (a kernel VMID:
 * MES owns 8-15) gets its own four-level table (gmc12.c) holding only the
 * write_42 shader, its IBs and an output page, all system pages. The same
 * kernel compute queue as 4e runs the IBs with VMID 1 in the
 * INDIRECT_BUFFER packet (gfx_v12_0_ring_emit_ib_compute); the ring, MQD
 * and fence stay in VMID 0. Three checks:
 *
 *  A. write_42 at VM_DATA_VA through VMID 1's table: 64 x 42.
 *  B. the same shader told to write at VMX_DATA_VA, which VMID 0 (the
 *     GART) maps to a poisoned victim page and VMID 1 does not map: the
 *     victim must stay poisoned, and GFXHUB must latch a VMID 1 fault at
 *     that address.
 *  C. revoke: VM_DATA_VA unmapped and VMID 1 flushed, then A's IB again:
 *     the output page must stay poisoned, with a VMID 1 fault there.
 *
 * Faults go to the default (dummy) page with no retry, so B and C still
 * reach their fence.
 */
#define VMT_GART_VA		0x00050000ull	/* after the 4e block */
#define VMT_VICTIM		0x3000u
#define VMT_SIZE		0x4000u
#define VMT_VMID		1u
#define VMT_PAGES		3u		/* shader, IBs, out */
#define VMT_IB_VMX_OFF		0x400u		/* in the IB page */
#define VMT_SEQ			0x5b000001u

static int
vmt_submit(struct mes *m, struct disp *d, u_int *n, uint64_t ib, u_int vmid,
    uint32_t seq, const char *what, int timeout_us)
{
	volatile uint32_t *ring = (volatile uint32_t *)(d->blk + DISP_RING);
	uint64_t fence = d->va + DISP_WB + 0x10;
	u_int k = *n;
	int us;

	ring[k++] = PKT3(IT_INDIRECT_BUFFER, 2);
	ring[k++] = (uint32_t)ib;
	ring[k++] = (uint32_t)(ib >> 32);
	ring[k++] = IB_VALID | W42_IB_DW | (vmid & 0xf) << 24;
	ring[k++] = PKT3(IT_RELEASE_MEM, 6);
	ring[k++] = RM_GCR_SEQ | RM_GCR_GL2_WB | RM_CACHE_POLICY(3) |
	    RM_EVENT_TYPE(CACHE_FLUSH_AND_INV_TS_EVENT) | RM_EVENT_INDEX(5);
	ring[k++] = RM_DATA_SEL(1) | RM_INT_SEL(2);
	ring[k++] = (uint32_t)fence;
	ring[k++] = (uint32_t)(fence >> 32);
	ring[k++] = seq;
	ring[k++] = 0;
	ring[k++] = 0;
	*n = k;
	*(volatile uint64_t *)(d->blk + DISP_WB + 8) = k;
	wmb();
	bus_write_8(m->dbbar, (bus_size_t)KCQ_DOORBELL * 4, k);
	for (us = 0; us < timeout_us; us += 10) {
		if (*(volatile uint32_t *)(d->blk + DISP_WB + 0x10) == seq)
			break;
		DELAY(10);
	}
	if (us >= timeout_us) {
		device_printf(m->sc->dev, "vm: %s: no fence in %d ms\n", what,
		    timeout_us / 1000);
		kcq_report(m, d, "vm fence timeout");
		amd_gmc12_fault_report(m->gt, "vm fence timeout");
		return (ETIMEDOUT);
	}
	device_printf(m->sc->dev, "vm: %s: fence %#x after %d us\n", what,
	    seq, us);
	return (0);
}

/* Count words of `p` that are not `want`. */
static u_int
vmt_count_not(volatile uint32_t *p, u_int dw, uint32_t want)
{
	u_int i, bad = 0;

	for (i = 0; i < dw; i++)
		if (p[i] != want)
			bad++;
	return (bad);
}

/* The fault a check expects: VMID 1, at `va`'s page. Logs what it saw. */
static bool
vmt_fault_is(struct mes *m, const char *what, uint64_t va)
{
	uint32_t st;
	uint64_t a;

	if (!amd_gmc12_fault_take(m->gt, &st, &a)) {
		device_printf(m->sc->dev, "vm: %s: no fault latched\n", what);
		return (va == 0);
	}
	device_printf(m->sc->dev, "vm: %s: GFXHUB fault status %#x (VMID %u, "
	    "client %#x, %s) at %#jx\n", what, st, amd_gmc12_fault_vmid(st),
	    (st >> 9) & 0x1ff, (st & (1u << 18)) ? "write" : "read",
	    (uintmax_t)a);
	return (va != 0 && amd_gmc12_fault_vmid(st) == VMT_VMID &&
	    a == (va & ~(uint64_t)PAGE_MASK));
}

/* Fresh ring and fence, MQD, ADD_QUEUE. */
static int
vmt_qopen(struct mes *m, struct disp *d)
{
	int err;

	memset(d->blk, 0, DISP_CODE);		/* ring, MQD, EOP, WB */
	kcq_mqd_init(d);
	wmb();
	if ((err = kcq_map(m, d)) != 0)
		kcq_report(m, d, "vm ADD_QUEUE failed");
	return (err);
}

/* REMOVE_QUEUE and HQD dequeue; true once the queue is known gone. */
static bool
vmt_qclose(struct mes *m, struct disp *d)
{
	if (kcq_unmap(m) != 0)
		kcq_report(m, d, "vm REMOVE_QUEUE failed");
	return (hqd_dequeue_me(m, 1, 0));
}

/*
 * A write VMID 1 must not be able to make (B, C). The wave faults (no
 * retry) and on gfx12 then stays stuck rather than finishing on the
 * dummy page (run T221309Z), so the dispatch does not fence by itself.
 * Pass: the fault is latched for VMID 1 at `va`'s page, `buf` stayed
 * poisoned, and the queue recovers: it fences on its own or after the
 * stuck waves of VMID 1 are killed (SQ_CMD). *errp is set only when the
 * queue did not recover (the caller must then remove it).
 */
#define VMT_FAULT_WAIT_US	200000

static bool
vmt_fault_case(struct mes *m, struct disp *d, u_int *n, uint64_t ib,
    uint32_t seq, const char *what, uint64_t va, volatile uint32_t *buf,
    int *errp)
{
	struct atrium_amd_softc *sc = m->sc;
	bool fenced, faulted, untouched, recovered = false;
	u_int bad;
	int us, kerr = 0;

	*errp = 0;
	fenced = vmt_submit(m, d, n, ib, VMT_VMID, seq, what,
	    VMT_FAULT_WAIT_US) == 0;
	faulted = vmt_fault_is(m, what, va);
	bad = vmt_count_not(buf, W42_OUT_DW, DISP_POISON);
	untouched = (bad == 0);
	if (!fenced) {
		kerr = amd_gfx12_sq_kill_vmid(sc, VMT_VMID);
		for (us = 0; us < DISP_TIMEOUT_US; us += 10) {
			if (*(volatile uint32_t *)(d->blk + DISP_WB + 0x10) ==
			    seq)
				break;
			DELAY(10);
		}
		recovered = (kerr == 0 && us < DISP_TIMEOUT_US);
		device_printf(sc->dev, "vm: %s: killed VMID %u's waves (%d): "
		    "%s\n", what, VMT_VMID, kerr, recovered ?
		    "fence arrived, queue recovered" : "still no fence");
		kcq_report(m, d, recovered ? "after wave kill" :
		    "wave kill did not recover");
		if (!recovered)
			*errp = ETIMEDOUT;
		if (vmt_fault_is(m, "after the kill", 0) == false)
			device_printf(sc->dev, "vm: %s: (fault above came after "
			    "the first one)\n", what);
	}
	if (faulted && untouched && (fenced || recovered))
		device_printf(sc->dev, "vm: %s: PASS, target page untouched, "
		    "VMID %u faulted at %#jx, queue %s\n", what, VMT_VMID,
		    (uintmax_t)va, fenced ? "fenced" : "recovered");
	else
		device_printf(sc->dev, "vm: %s: FAILED (fault %s, %u target "
		    "words changed, %s)\n", what, faulted ? "right" :
		    "missing or wrong", bad, fenced ? "fenced" : recovered ?
		    "recovered" : "queue stuck");
	return (faulted && untouched && (fenced || recovered));
}

static const char *const vmt_names[] = {
	[AMD_VM12_4L_VRAM] = "4-level, tables in VRAM (amdgpu layout)",
	[AMD_VM12_FLAT] = "flat, depth 0, GART-style PTEs",
	[AMD_VM12_4L_SYS] = "4-level, tables in system memory",
};

/*
 * One layout: build VMID 1's table, bind it, open the queue, run A and,
 * if A passes, B and C. Returns 0 when all three pass. *gone is false
 * when the queue could not be removed (the caller must then stop and
 * leak everything the GPU can still reach).
 */
static int
vmt_variant(struct mes *m, struct disp *d, uint8_t *pg, vm_paddr_t pg_pa,
    int mode, uint32_t *seq, bool *gone)
{
	struct atrium_amd_softc *sc = m->sc;
	struct amd_gart *gt = m->gt;
	struct amd_vm12 vm;
	volatile uint32_t *out = (volatile uint32_t *)(pg + 2 * PAGE_SIZE);
	volatile uint32_t *victim = (volatile uint32_t *)(d->blk + VMT_VICTIM);
	uint64_t ib_vm = W42_VM_CODE_VA + W42_IB_OFFSET;
	uint64_t ib_vmx = ib_vm + VMT_IB_VMX_OFF, a;
	const u_int rwx = AMD_VM_READ | AMD_VM_WRITE | AMD_VM_EXEC;
	const char *nm = vmt_names[mode];
	uint32_t st;
	u_int i, n = 0, bad, fails = 0;
	int err;

	*gone = true;
	device_printf(sc->dev, "vm: --- layout %d: %s ---\n", mode, nm);
	if ((err = amd_gmc12_vm_init(gt, &vm, mode, W42_VM_CODE_VA)) != 0 ||
	    (err = amd_gmc12_vm_map(&vm, W42_VM_CODE_VA, pg_pa, rwx)) != 0 ||
	    (err = amd_gmc12_vm_map(&vm, W42_VM_CODE_VA + PAGE_SIZE,
	    pg_pa + PAGE_SIZE, rwx)) != 0 ||
	    (err = amd_gmc12_vm_map(&vm, W42_VM_DATA_VA, pg_pa + 2 * PAGE_SIZE,
	    rwx)) != 0) {
		device_printf(sc->dev, "vm: %s: table build failed (%d)\n", nm,
		    err);
		amd_gmc12_vm_fini(&vm);
		return (err);
	}
	if (amd_gmc12_fault_take(gt, &st, &a))
		device_printf(sc->dev, "vm: stale fault %#x at %#jx "
		    "(cleared)\n", st, (uintmax_t)a);
	if ((err = amd_gmc12_vm_bind(gt, VMT_VMID, &vm)) != 0) {
		device_printf(sc->dev, "vm: bind/flush VMID %u: %d\n",
		    VMT_VMID, err);
		goto out;
	}
	device_printf(sc->dev, "vm: VMID %u base %#jx, %u table page(s)\n",
	    VMT_VMID, (uintmax_t)amd_gmc12_vm_pd_addr(&vm), vm.used);
	amd_gmc12_ctx_report(gt, VMT_VMID, "bound");
	amd_gmc12_vm_dump(&vm, ib_vm);
	amd_gmc12_vm_dump(&vm, W42_VM_DATA_VA);
	for (i = 0; i < W42_OUT_DW; i++) {
		out[i] = DISP_POISON;
		victim[i] = DISP_POISON;
	}
	if ((err = vmt_qopen(m, d)) != 0) {
		*gone = vmt_qclose(m, d);
		goto out;
	}

	/* A: VMID 1 writes its own page. */
	if ((err = vmt_submit(m, d, &n, ib_vm, VMT_VMID, (*seq)++,
	    "A (own page)", DISP_TIMEOUT_US)) != 0) {
		if (amd_gmc12_fault_take(gt, &st, &a)) {
			amd_gmc12_fault_decode(gt, st, "A");
			device_printf(sc->dev, "vm: A: fault page %#jx\n",
			    (uintmax_t)a);
		}
		amd_gmc12_ctx_report(gt, VMT_VMID, "A timeout");
		device_printf(sc->dev, "vm: layout %d (%s): A FAILED (no "
		    "fence)\n", mode, nm);
		*gone = vmt_qclose(m, d);
		goto out;
	}
	bad = vmt_count_not(out, W42_OUT_DW, W42_OUT_VALUE);
	if (bad == 0 && vmt_fault_is(m, "A", 0))
		device_printf(sc->dev, "vm: A: PASS, all %u words %#x through "
		    "VMID %u's own table\n", W42_OUT_DW, W42_OUT_VALUE,
		    VMT_VMID);
	else {
		device_printf(sc->dev, "vm: A: FAILED, %u of %u words wrong "
		    "(out[0] %#x)\n", bad, W42_OUT_DW, out[0]);
		fails++;
	}

	/* B: VMID 1 aims at a VA only VMID 0 maps. */
	if (!vmt_fault_case(m, d, &n, ib_vmx, (*seq)++,
	    "B (VA mapped only in VMID 0)", W42_VMX_DATA_VA, victim, &err))
		fails++;
	if (err != 0) {
		*gone = vmt_qclose(m, d);
		goto out;
	}

	/* C: revoke the out page, run A's IB again. */
	for (i = 0; i < W42_OUT_DW; i++)
		out[i] = DISP_POISON;
	if ((err = amd_gmc12_vm_unmap(&vm, W42_VM_DATA_VA)) != 0 ||
	    (err = amd_gmc12_vm_flush(gt, VMT_VMID)) != 0) {
		device_printf(sc->dev, "vm: C: unmap/flush failed (%d)\n", err);
		*gone = vmt_qclose(m, d);
		goto out;
	}
	if (!vmt_fault_case(m, d, &n, ib_vm, (*seq)++, "C (page revoked)",
	    W42_VM_DATA_VA, out, &err))
		fails++;
	if (err != 0) {
		*gone = vmt_qclose(m, d);
		goto out;
	}
	if (gt->dummy != NULL) {
		uint32_t w0 = *(volatile uint32_t *)gt->dummy;

		device_printf(sc->dev, "vm: dummy page word 0 now %#x (%s); "
		    "zeroed\n", w0, w0 == 0 ? "faulting writes did not land "
		    "there" : "a faulting write landed there");
		memset(gt->dummy, 0, PAGE_SIZE);
	}
	*gone = vmt_qclose(m, d);
	err = fails != 0 ? EIO : 0;
	device_printf(sc->dev, "vm: layout %d (%s): %s\n", mode, nm,
	    err == 0 ? "A, B, C PASS" : "FAILED");
out:
	(void)amd_gmc12_vm_bind(gt, VMT_VMID, NULL);
	if (*gone)
		amd_gmc12_vm_fini(&vm);	/* else system tables leak */
	return (err);
}

/*
 * The 5b test on the four-level VRAM layout. (Run T221309Z also ran the
 * flat and system-memory layouts; add them to modes[] to compare again.)
 */
static int
vm_test_run(struct mes *m)
{
	static const int modes[] = { AMD_VM12_4L_VRAM };
	struct atrium_amd_softc *sc = m->sc;
	const struct mes_gen *g = m->g;
	struct amd_gart *gt = m->gt;
	struct disp d;
	uint8_t *pg;
	vm_paddr_t pg_pa;
	uint32_t seq = VMT_SEQ;
	u_int i, k;
	int err, res[nitems(modes)];
	bool gone = true;

	memset(&d, 0, sizeof(d));
	d.va = gt->gart_start + VMT_GART_VA;
	if (d.va + VMT_VICTIM != W42_VMX_DATA_VA ||
	    d.va + VMT_SIZE > gt->gart_end + 1 || W42_IB_OFFSET != PAGE_SIZE ||
	    W42_VM_DATA_VA - W42_VM_CODE_VA >= (2u << 20)) {
		device_printf(sc->dev, "vm: write42_ib.h built for victim "
		    "%#jx, block at %#jx; regenerate it\n",
		    (uintmax_t)W42_VMX_DATA_VA, (uintmax_t)d.va);
		return (EINVAL);
	}
	/* VMID 1's pages: shader, IBs, out. */
	pg = contigmalloc(VMT_PAGES * PAGE_SIZE, M_DEVBUF, M_WAITOK | M_ZERO,
	    0, ~(vm_paddr_t)0, PAGE_SIZE, 0);
	pg_pa = vtophys(pg);
	memcpy(pg, w42_shader, sizeof(w42_shader));
	memcpy(pg + PAGE_SIZE, w42_ib_vm, sizeof(w42_ib_vm));
	memcpy(pg + PAGE_SIZE + VMT_IB_VMX_OFF, w42_ib_vmx, sizeof(w42_ib_vmx));

	/* The kernel queue's block in the GART, with the victim page. */
	d.blk = contigmalloc(VMT_SIZE, M_DEVBUF, M_WAITOK | M_ZERO, 0,
	    ~(vm_paddr_t)0, PAGE_SIZE, 0);
	d.pa = vtophys(d.blk);
	for (i = 0; i < VMT_SIZE / PAGE_SIZE; i++)
		(void)amd_gmc12_gart_map(gt, d.va + i * PAGE_SIZE,
		    d.pa + i * PAGE_SIZE);
	device_printf(sc->dev, "vm: pages pa %#jx (shader %#jx, IBs "
	    "%#jx/%#jx, out %#jx); queue block GART %#jx, victim %#jx\n",
	    (uintmax_t)pg_pa, (uintmax_t)W42_VM_CODE_VA,
	    (uintmax_t)(W42_VM_CODE_VA + W42_IB_OFFSET),
	    (uintmax_t)(W42_VM_CODE_VA + W42_IB_OFFSET + VMT_IB_VMX_OFF),
	    (uintmax_t)W42_VM_DATA_VA, (uintmax_t)d.va,
	    (uintmax_t)W42_VMX_DATA_VA);
	if ((err = amd_gmc12_flush(gt)) != 0)
		goto out_unmap;
	grbm_select(m, 1, 0);
	d.int_cntl = rd(m, g->cpc_int_cntl);
	wr(m, g->cpc_int_cntl, d.int_cntl | TIME_STAMP_INT_ENABLE);
	grbm_select(m, 0, 0);

	for (k = 0; k < nitems(modes); k++)
		res[k] = ENXIO;
	for (k = 0; k < nitems(modes) && gone; k++)
		res[k] = vmt_variant(m, &d, pg, pg_pa, modes[k], &seq, &gone);
	for (k = 0; k < nitems(modes); k++)
		device_printf(sc->dev, "vm: summary: layout %d (%s): %s\n",
		    modes[k], vmt_names[modes[k]], res[k] == 0 ? "PASS" :
		    res[k] == ENXIO ? "not run" : "FAILED");
	err = res[0];

	grbm_select(m, 1, 0);
	wr(m, g->cpc_int_cntl, d.int_cntl);
	grbm_select(m, 0, 0);
out_unmap:
	for (i = 0; i < VMT_SIZE / PAGE_SIZE; i++) {
		uint64_t idx = (d.va - gt->gart_start) / PAGE_SIZE + i;

		bus_write_4(gt->vram, gt->table_off + idx * 8, 0);
		bus_write_4(gt->vram, gt->table_off + idx * 8 + 4, 0);
	}
	(void)amd_gmc12_flush(gt);
	if (gone) {
		contigfree(d.blk, VMT_SIZE, M_DEVBUF);
		contigfree(pg, VMT_PAGES * PAGE_SIZE, M_DEVBUF);
	} else
		device_printf(sc->dev, "vm: queue still active; its blocks "
		    "are leaked, not freed\n");
	device_printf(sc->dev, "vm: stage 5b: %s\n", err == 0 ?
	    "PASS (own table, cross-VMID write blocked, revoke works)" :
	    "FAILED");
	return (err);
}

/*
 * Stage 5c-2: the kernel compute queue that client SUBMITs run on (the
 * client node, si_client.c). The 4e/5b queue (MEC1 pipe 0 queue 0, VMID
 * 0, the KCQ doorbell) in a block of its own, kept mapped while the client
 * node is up. A submit is an INDIRECT_BUFFER under the client VM's kernel
 * VMID (1-7) and a RELEASE_MEM that writes the next sequence number to the
 * fence and raises an EOP interrupt. The ring and the fence are in VMID 0,
 * which nothing a client's IB does can reach, so the fence is the truth
 * about completion: an EOP interrupt a client raises with a RELEASE_MEM of
 * its own does not move it. The attach self-tests (4e, 5b) use the same
 * MEC slot and doorbell, before this queue is opened and after it closes.
 * Callers serialise (si_client.c holds its queue lock).
 */
#define KQ_GART_VA		0x00070000ull	/* after 5c-1's blocks */
#define KQ_SIZE			DISP_CODE	/* ring, MQD, EOP, WB */
#define KQ_PKT_DW		12u

struct amd_kq {
	struct mes *m;
	struct disp d;
	uint64_t wptr;		/* dwords emitted since the queue was mapped */
	uint32_t seq;		/* last sequence number emitted */
	uint32_t gen;		/* bumped at each reset (seq starts over) */
	bool mapped;
};
/* A fill: DMA_DATA packets of KQ_FILL_CHUNK, then the fence. */
#define KQ_FILL_CHUNK		(32ull << 20)
#define KQ_FILL_DW		((u_int)(AMD_KQ_FILL_MAX / KQ_FILL_CHUNK) * 7 + 8)
CTASSERT(KQ_FILL_CHUNK <= DMA_DATA_MAX_BYTES);

static void
gart_unmap_blk(struct amd_gart *gt, uint64_t va, u_int size)
{
	u_int i;

	for (i = 0; i < size / PAGE_SIZE; i++) {
		uint64_t idx = (va - gt->gart_start) / PAGE_SIZE + i;

		bus_write_4(gt->vram, gt->table_off + idx * 8, 0);
		bus_write_4(gt->vram, gt->table_off + idx * 8 + 4, 0);
	}
	(void)amd_gmc12_flush(gt);
}

static void
kq_int(struct amd_kq *kq, bool on)
{
	struct mes *m = kq->m;

	grbm_select(m, 1, 0);
	if (on) {
		kq->d.int_cntl = rd(m, m->g->cpc_int_cntl);
		wr(m, m->g->cpc_int_cntl, kq->d.int_cntl |
		    TIME_STAMP_INT_ENABLE);
	} else
		wr(m, m->g->cpc_int_cntl, kq->d.int_cntl);
	grbm_select(m, 0, 0);
}

int
amd_mes12_kq_open(struct mes *m, struct amd_kq **out)
{
	struct amd_gart *gt = m->gt;
	struct amd_kq *kq;
	u_int i;
	int err;

	*out = NULL;
	kq = malloc(sizeof(*kq), M_DEVBUF, M_WAITOK | M_ZERO);
	kq->m = m;
	kq->d.va = gt->gart_start + KQ_GART_VA;
	if (kq->d.va + KQ_SIZE > gt->gart_end + 1) {
		free(kq, M_DEVBUF);
		return (EINVAL);
	}
	kq->d.blk = contigmalloc(KQ_SIZE, M_DEVBUF, M_WAITOK | M_ZERO, 0,
	    ~(vm_paddr_t)0, PAGE_SIZE, 0);
	kq->d.pa = vtophys(kq->d.blk);
	for (i = 0; i < KQ_SIZE / PAGE_SIZE; i++)
		(void)amd_gmc12_gart_map(gt, kq->d.va + i * PAGE_SIZE,
		    kq->d.pa + i * PAGE_SIZE);
	if ((err = amd_gmc12_flush(gt)) != 0)
		goto fail;
	kq_int(kq, true);
	if ((err = vmt_qopen(m, &kq->d)) != 0) {
		kq_int(kq, false);
		if (!hqd_dequeue_me(m, 1, 0)) {
			/* The HQD may still use the block: leak it. */
			gart_unmap_blk(gt, kq->d.va, KQ_SIZE);
			device_printf(m->sc->dev, "kq: ADD_QUEUE failed and "
			    "the HQD is still active; block leaked\n");
			free(kq, M_DEVBUF);
			return (err);
		}
		goto fail;
	}
	kq->mapped = true;
	device_printf(m->sc->dev, "kq: client queue up: MEC1 pipe 0 queue 0, "
	    "block at GART %#jx, doorbell %#x\n", (uintmax_t)kq->d.va,
	    KCQ_DOORBELL);
	*out = kq;
	return (0);
fail:
	gart_unmap_blk(gt, kq->d.va, KQ_SIZE);
	contigfree(kq->d.blk, KQ_SIZE, M_DEVBUF);
	free(kq, M_DEVBUF);
	return (err);
}

void
amd_mes12_kq_close(struct amd_kq *kq)
{
	struct amd_gart *gt;
	bool gone = true;

	if (kq == NULL)
		return;
	gt = kq->m->gt;
	if (kq->mapped) {
		gone = vmt_qclose(kq->m, &kq->d);
		kq_int(kq, false);
	}
	gart_unmap_blk(gt, kq->d.va, KQ_SIZE);
	if (gone)
		contigfree(kq->d.blk, KQ_SIZE, M_DEVBUF);
	else
		device_printf(kq->m->sc->dev, "kq: queue still active at "
		    "close; its block is leaked, not freed\n");
	free(kq, M_DEVBUF);
}

/*
 * The queue is wedged (a fence that never came): remove it and map it
 * afresh, ring and fence zeroed. Sequence numbers start over.
 */
int
amd_mes12_kq_reset(struct amd_kq *kq)
{
	struct mes *m = kq->m;
	int err;

	atomic_add_rel_32(&kq->gen, 1);	/* before the fence goes back to 0 */
	if (kq->mapped) {
		kq->mapped = false;
		if (!vmt_qclose(m, &kq->d)) {
			device_printf(m->sc->dev, "kq: reset: HQD still "
			    "active; queue left down\n");
			return (EIO);
		}
	}
	kq->wptr = 0;
	kq->seq = 0;
	if ((err = vmt_qopen(m, &kq->d)) != 0)
		return (err);
	kq->mapped = true;
	device_printf(m->sc->dev, "kq: reset: queue mapped afresh\n");
	return (0);
}

/*
 * Emit one job: IB at `ib` (`ib_dw` dwords) under `vmid`, then the fence
 * and EOP interrupt; ring the doorbell. *seq is the job's fence value.
 * EBUSY if the ring has no room (more than a ring's worth in flight).
 */
int
amd_mes12_kq_submit(struct amd_kq *kq, uint64_t ib, uint32_t ib_dw,
    u_int vmid, uint32_t *seq)
{
	volatile uint32_t *ring = (volatile uint32_t *)(kq->d.blk + DISP_RING);
	uint64_t fence = kq->d.va + DISP_WB + 0x10;
	uint32_t pkt[KQ_PKT_DW], s, inflight;
	u_int i, n = 0;

	if (!kq->mapped)
		return (ENXIO);
	if (ib_dw == 0 || ib_dw > 0xfffffu || vmid == 0 || vmid > 15 ||
	    (ib & 3) != 0)
		return (EINVAL);
	inflight = kq->seq - amd_mes12_kq_fence(kq);
	/* Room for one fill too: a fill goes in only on an idle queue, so
	 * at most one is ahead of these. */
	if ((inflight + 1) * KQ_PKT_DW + KQ_FILL_DW >= RING_DW)
		return (EBUSY);
	s = kq->seq + 1;
	pkt[n++] = PKT3(IT_INDIRECT_BUFFER, 2);
	pkt[n++] = (uint32_t)ib;
	pkt[n++] = (uint32_t)(ib >> 32);
	pkt[n++] = IB_VALID | ib_dw | (vmid & 0xf) << 24;
	pkt[n++] = PKT3(IT_RELEASE_MEM, 6);
	pkt[n++] = RM_GCR_SEQ | RM_GCR_GL2_WB | RM_CACHE_POLICY(3) |
	    RM_EVENT_TYPE(CACHE_FLUSH_AND_INV_TS_EVENT) | RM_EVENT_INDEX(5);
	pkt[n++] = RM_DATA_SEL(1) | RM_INT_SEL(2);
	pkt[n++] = (uint32_t)fence;
	pkt[n++] = (uint32_t)(fence >> 32);
	pkt[n++] = s;
	pkt[n++] = 0;
	pkt[n++] = AMD_KQ_CTXID;			/* INT_CTXID */
	for (i = 0; i < n; i++)
		ring[(kq->wptr + i) & (RING_DW - 1)] = pkt[i];
	kq->wptr += n;
	kq->seq = s;
	*(volatile uint64_t *)(kq->d.blk + DISP_WB + 8) = kq->wptr;
	wmb();
	bus_write_8(kq->m->dbbar, (bus_size_t)KCQ_DOORBELL * 4, kq->wptr);
	*seq = s;
	return (0);
}

/*
 * 5d: zero [mc, mc + bytes) with CP DMA (DMA_DATA, SRC_SEL DATA, through
 * the L2) on the ring itself, so under VMID 0: `mc` is an MC address, VRAM
 * at fb_start + offset through the system aperture, which no client VMID
 * maps. CP_SYNC holds each fill until it is done; the RELEASE_MEM after
 * them writes the L2 back and the fence (no interrupt: nothing waits on an
 * IV for this). Only on an idle queue (EBUSY otherwise), so the ring
 * accounting in amd_mes12_kq_submit stays one fill plus its packets.
 */
int
amd_mes12_kq_fill(struct amd_kq *kq, uint64_t mc, uint64_t bytes,
    uint32_t *seq, uint32_t *gen)
{
	volatile uint32_t *ring = (volatile uint32_t *)(kq->d.blk + DISP_RING);
	uint64_t fence = kq->d.va + DISP_WB + 0x10, done, chunk;
	uint32_t pkt[KQ_FILL_DW], s;
	u_int i, n = 0;

	/* The generation first: a reset from here on shows as a change. */
	*gen = atomic_load_acq_32(&kq->gen);
	if (!kq->mapped)
		return (ENXIO);
	if (bytes == 0 || bytes > AMD_KQ_FILL_MAX || (bytes & 3) != 0 ||
	    (mc & 3) != 0)
		return (EINVAL);
	if (kq->seq != amd_mes12_kq_fence(kq))
		return (EBUSY);
	s = kq->seq + 1;
	for (done = 0; done < bytes; done += chunk) {
		chunk = MIN(bytes - done, KQ_FILL_CHUNK);
		pkt[n++] = PKT3(IT_DMA_DATA, 5);
		pkt[n++] = DMA_DATA_CP_SYNC | DMA_DATA_SRC_SEL_DATA |
		    DMA_DATA_DST_SEL_L2;
		pkt[n++] = 0;				/* the fill value */
		pkt[n++] = 0;
		pkt[n++] = (uint32_t)(mc + done);
		pkt[n++] = (uint32_t)((mc + done) >> 32);
		pkt[n++] = (uint32_t)chunk;		/* BYTE_COUNT */
	}
	pkt[n++] = PKT3(IT_RELEASE_MEM, 6);
	pkt[n++] = RM_GCR_SEQ | RM_GCR_GL2_WB | RM_CACHE_POLICY(3) |
	    RM_EVENT_TYPE(CACHE_FLUSH_AND_INV_TS_EVENT) | RM_EVENT_INDEX(5);
	pkt[n++] = RM_DATA_SEL(1);			/* no interrupt */
	pkt[n++] = (uint32_t)fence;
	pkt[n++] = (uint32_t)(fence >> 32);
	pkt[n++] = s;
	pkt[n++] = 0;
	pkt[n++] = 0;
	for (i = 0; i < n; i++)
		ring[(kq->wptr + i) & (RING_DW - 1)] = pkt[i];
	kq->wptr += n;
	kq->seq = s;
	*(volatile uint64_t *)(kq->d.blk + DISP_WB + 8) = kq->wptr;
	wmb();
	bus_write_8(kq->m->dbbar, (bus_size_t)KCQ_DOORBELL * 4, kq->wptr);
	*seq = s;
	return (0);
}

uint32_t
amd_mes12_kq_gen(struct amd_kq *kq)
{
	return (atomic_load_acq_32(&kq->gen));
}

uint32_t
amd_mes12_kq_fence(struct amd_kq *kq)
{
	return (*(volatile uint32_t *)(kq->d.blk + DISP_WB + 0x10));
}

void
amd_mes12_kq_report(struct amd_kq *kq, const char *when)
{
	device_printf(kq->m->sc->dev, "kq: %s: seq %u, fence %u, wptr %ju\n",
	    when, kq->seq, amd_mes12_kq_fence(kq), (uintmax_t)kq->wptr);
	kcq_report(kq->m, &kq->d, when);
}

/*
 * Stage 5c, step 1: MES user queues on silicon, driven from the kernel
 * (the fd/kqueue interface comes on top in step 2). Two "processes", each
 * with its own four-level VM (5b) and one compute user queue added with a
 * plain ADD_QUEUE (mes_userq_map: process and gang context pages, quanta
 * 10 ms / 1 ms, NORMAL priority, the VM's page-table base, VA range
 * 0..max_pfn-1, vm_context_cntl = the CONTEXT1-15 setting). MES picks the
 * HQD and the VMID (8-15). The MQD is gfx_v12_0_compute_mqd_init for a
 * user queue: no PRIV_STATE/KMD_QUEUE, and the ring, EOP and rptr/wptr
 * are VAs in the process's own VM; the MQD and the context pages are in
 * the GART, and the wptr page is aliased there for MES (wptr_mc_addr).
 *
 * Both processes map write_42 at the SAME VAs (UQ_VA + ...) onto their
 * own pages (each poisoned differently). Pass: both fences, each
 * process's out page all 42, the two queues on different VMIDs (read from
 * the HQDs after both ran: MES maps a queue when it has work), and after
 * REMOVE_QUEUE no HQD left active.
 */
#define UQ_N			2
#define UQ_VA			0x200000000ull	/* in each process's VM */
#define UQ_RING			0x0000u
#define UQ_WB			0x1000u		/* rptr 0, wptr 8, fence 0x10 */
#define UQ_EOP			0x2000u
#define UQ_CODE			0x3000u		/* shader; IB page next */
#define UQ_IB			0x4000u
#define UQ_OUT			0x5000u
#define UQ_PAGES		6u
#define UQ_GART_VA		0x00060000ull	/* per process: 0x4000 apart */
#define UQ_G_MQD		0x0000u
#define UQ_G_PROC		0x1000u
#define UQ_G_GANG		0x2000u
#define UQ_G_WB			0x3000u		/* alias of the WB page */
#define UQ_G_SIZE		0x4000u
#define UQ_DOORBELL(p)		AMD_UQ_DOORBELL(p)	/* userqueue range */
#define UQ_SEQ			0x5c000001u

struct uqp {
	struct amd_vm12 vm;
	uint8_t	*pg;			/* UQ_PAGES: the process's pages */
	vm_paddr_t pg_pa;
	uint8_t	*g;			/* MQD, process ctx, gang ctx */
	vm_paddr_t g_pa;
	uint64_t gva;			/* GART VA of g */
	bool	vm_up, added;
};

static void
grbm_select_q(struct mes *m, u_int me, u_int pipe, u_int queue)
{
	wr(m, m->g->grbm_gfx_cntl, (pipe & 3) | (me & 3) << 2 |
	    (queue & 7) << 8);
}

/*
 * gfx_v12_0_compute_mqd_init for a user queue (prop->kernel_queue false):
 * the MQD at GART `mqd`, ring/EOP/writeback at VAs of the process's VM,
 * doorbell dword index `db`.
 */
static void
uq_mqd_build(uint8_t *q, uint64_t mqd, uint64_t ring, uint64_t eop,
    uint64_t wb, uint32_t db)
{
	uint32_t t;

	memset(q, 0, MQD_SIZE);
	put32(q, MQD_HEADER, 0xC0310800u);
	put32(q, MQD_PIPELINESTAT, 1);
	put32(q, MQD_STM_SE0, ~0u);
	put32(q, MQD_STM_SE1, ~0u);
	put32(q, MQD_STM_SE2, ~0u);
	put32(q, MQD_STM_SE3, ~0u);
	put32(q, MQD_MISC_RESERVED, 7);
	put32(q, MQD_EOP_BASE_LO, (uint32_t)(eop >> 8));
	put32(q, MQD_EOP_BASE_HI, (uint32_t)(eop >> 40));
	put32(q, MQD_EOP_CONTROL, (EOP_CONTROL_DEFAULT & ~0x3fu) | 8);
	put32(q, MQD_MQD_BASE_LO, (uint32_t)mqd & 0xfffffffcu);
	put32(q, MQD_MQD_BASE_HI, (uint32_t)(mqd >> 32));
	put32(q, MQD_MQD_CONTROL, MQD_CONTROL_DEFAULT);	/* MQD in VMID 0 */
	put32(q, MQD_PQ_BASE_LO, (uint32_t)(ring >> 8));
	put32(q, MQD_PQ_BASE_HI, (uint32_t)(ring >> 40));
	/* QUEUE_SIZE, RPTR_BLOCK_SIZE, UNORD_DISPATCH; no PRIV_STATE/KMD. */
	t = PQ_CONTROL_DEFAULT & ~(0x3fu | 0x3f00u);
	t |= (uint32_t)(ffs(RING_DW) - 2) | (9u << 8);
	t |= 1u << 28;
	put32(q, MQD_PQ_CONTROL, t);
	put32(q, MQD_RPTR_REPORT_LO, (uint32_t)wb & 0xfffffffcu);
	put32(q, MQD_RPTR_REPORT_HI, (uint32_t)(wb >> 32) & 0xffff);
	put32(q, MQD_WPTR_POLL_LO, (uint32_t)(wb + 8) & 0xfffffffcu);
	put32(q, MQD_WPTR_POLL_HI, (uint32_t)((wb + 8) >> 32) & 0xffff);
	put32(q, MQD_DOORBELL, (db << 2) | (1u << 30));
	put32(q, MQD_PQ_RPTR, 0);
	put32(q, MQD_HQD_VMID, 0);			/* MES sets it */
	put32(q, MQD_PERSISTENT, (PERSISTENT_DEFAULT & ~0x3ff00u) |
	    (0x55u << 8));
	put32(q, MQD_IB_CONTROL, IB_CONTROL_DEFAULT);
	put32(q, MQD_QUANTUM, 0x111u);
	put32(q, MQD_HQD_ACTIVE, 0);
}

static void
uq_mqd_init(struct uqp *u, u_int p)
{
	uq_mqd_build(u->g + UQ_G_MQD, u->gva + UQ_G_MQD, UQ_VA + UQ_RING,
	    UQ_VA + UQ_EOP, UQ_VA + UQ_WB, UQ_DOORBELL(p));
}

/*
 * mes_userq_map's ADD_QUEUE: one gang (context at gva + UQ_G_GANG) of a
 * process (context at gva + UQ_G_PROC) whose page tables are `vm`; the MQD
 * at gva + UQ_G_MQD and MES's wptr alias at gva + UQ_G_WB + 8.
 */
static int
uq_add_raw(struct mes *m, struct amd_vm12 *vm, uint64_t gva, uint32_t pid,
    uint32_t db, uint32_t prio)
{
	uint32_t pkt[API_DW];
	uint8_t *b = (uint8_t *)pkt;

	memset(pkt, 0, sizeof(pkt));
	pkt[0] = HDR_ADD_QUEUE;
	put32(b, AQ_PROCESS_ID, pid);			/* a pasid-like id */
	put64(b, AQ_PTB, amd_gmc12_vm_pd_addr(vm));
	put64(b, AQ_VA_START, 0);
	put64(b, AQ_VA_END, (1ull << 36) - 1);		/* max_pfn - 1 */
	put64(b, AQ_PROC_QUANTUM, 100000);		/* 10 ms */
	put64(b, AQ_PROC_CTX, gva + UQ_G_PROC);
	put64(b, AQ_GANG_QUANTUM, 10000);		/* 1 ms */
	put64(b, AQ_GANG_CTX, gva + UQ_G_GANG);
	put32(b, AQ_INPROC_PRIO, prio);
	put32(b, AQ_GANG_PRIO, prio);
	put32(b, AQ_DOORBELL, db);
	put64(b, AQ_MQD_ADDR, gva + UQ_G_MQD);
	put64(b, AQ_WPTR_ADDR, gva + UQ_G_WB + 8);
	put32(b, AQ_QUEUE_TYPE, QUEUE_TYPE_COMPUTE);
	put32(b, AQ_GDS_SIZE, RING_DW);
	put32(b, AQ_VM_CNTX_CNTL, amd_gmc12_ctx_cntl(m->gt, 8));
	return (mes_api(m, 0, pkt, AQ_API_STATUS, "ADD_QUEUE (user compute)"));
}

static int
uq_remove_raw(struct mes *m, uint64_t gva, uint32_t db)
{
	uint32_t pkt[API_DW];
	uint8_t *b = (uint8_t *)pkt;

	memset(pkt, 0, sizeof(pkt));
	pkt[0] = HDR_REMOVE_QUEUE;
	put32(b, RQ_DOORBELL, db);
	put64(b, RQ_GANG_CTX, gva + UQ_G_GANG);
	put32(b, RQ_QUEUE_TYPE, QUEUE_TYPE_COMPUTE);
	return (mes_api(m, 0, pkt, RQ_API_STATUS,
	    "REMOVE_QUEUE (user compute)"));
}

/*
 * MES's own reset of one queue (MES_SCH_API_RESET, reset_queue_only, by
 * doorbell), as amdgpu's mes_v12_0_reset_hw_queue: for a queue whose HQD
 * a direct reset did not stop and whose REMOVE_QUEUE MES refused.
 */
static int
uq_reset_raw(struct mes *m, uint64_t gva, uint32_t db)
{
	uint32_t pkt[API_DW];
	uint8_t *b = (uint8_t *)pkt;

	memset(pkt, 0, sizeof(pkt));
	pkt[0] = HDR_RESET;
	put32(b, RST_FLAGS, 1);
	put64(b, RST_GANG_CTX, gva + UQ_G_GANG);
	put32(b, RST_DOORBELL, db);
	put32(b, RST_QUEUE_TYPE, QUEUE_TYPE_COMPUTE);
	return (mes_api(m, 0, pkt, RST_API_STATUS,
	    "RESET (user compute queue)"));
}

static int
uq_add(struct mes *m, struct uqp *u, u_int p)
{
	return (uq_add_raw(m, &u->vm, u->gva, 0x100 + p, UQ_DOORBELL(p), 1));
}

static int
uq_remove(struct mes *m, struct uqp *u, u_int p)
{
	return (uq_remove_raw(m, u->gva, UQ_DOORBELL(p)));
}

/*
 * Every MEC1 compute HQD (pipes 0-1, queues 0-3): active ones with their
 * VMID and doorbell. Returns how many are active; vmids[] gets the VMID
 * of the HQD whose doorbell is UQ_DOORBELL(p).
 */
static u_int
uq_hqd_scan(struct mes *m, const char *when, u_int *vmids)
{
	const struct mes_gen *g = m->g;
	uint32_t act, vmid, db;
	u_int pipe, q, p, n = 0;

	for (pipe = 0; pipe < 2; pipe++)
		for (q = 0; q < 4; q++) {
			grbm_select_q(m, 1, pipe, q);
			act = rd(m, g->hqd_active) & 1;
			vmid = rd(m, g->hqd_vmid) & 0xf;
			db = rd(m, g->hqd_doorbell);
			if (!act)
				continue;
			n++;
			device_printf(m->sc->dev, "uq: %s: HQD me1 pipe %u queue "
			    "%u active, VMID %u, doorbell %#x\n", when, pipe, q,
			    vmid, db);
			for (p = 0; p < UQ_N && vmids != NULL; p++)
				if (((db >> 2) & 0x03ffffff) == UQ_DOORBELL(p))
					vmids[p] = vmid;
		}
	grbm_select_q(m, 0, 0, 0);
	return (n);
}

static int
uq_setup(struct mes *m, struct uqp *u, u_int p)
{
	struct amd_gart *gt = m->gt;
	const u_int rwx = AMD_VM_READ | AMD_VM_WRITE | AMD_VM_EXEC;
	u_int i;
	int err;

	u->pg = contigmalloc(UQ_PAGES * PAGE_SIZE, M_DEVBUF, M_WAITOK | M_ZERO,
	    0, ~(vm_paddr_t)0, PAGE_SIZE, 0);
	u->pg_pa = vtophys(u->pg);
	u->g = contigmalloc(UQ_G_SIZE, M_DEVBUF, M_WAITOK | M_ZERO, 0,
	    ~(vm_paddr_t)0, PAGE_SIZE, 0);
	u->g_pa = vtophys(u->g);
	u->gva = gt->gart_start + UQ_GART_VA + (uint64_t)p * UQ_G_SIZE;
	memcpy(u->pg + UQ_CODE, w42_shader, sizeof(w42_shader));
	memcpy(u->pg + UQ_IB, w42_ib_uq, sizeof(w42_ib_uq));
	for (i = 0; i < W42_OUT_DW; i++)
		((volatile uint32_t *)(u->pg + UQ_OUT))[i] = DISP_POISON + p;

	/* GART: MQD, process ctx, gang ctx, and the WB page's alias. */
	for (i = 0; i < 3; i++)
		(void)amd_gmc12_gart_map(gt, u->gva + i * PAGE_SIZE,
		    u->g_pa + i * PAGE_SIZE);
	(void)amd_gmc12_gart_map(gt, u->gva + UQ_G_WB, u->pg_pa + UQ_WB);

	/* The process's own VM: all six pages at UQ_VA. */
	if ((err = amd_gmc12_vm_init(gt, &u->vm, AMD_VM12_4L_VRAM, 0)) != 0)
		return (err);
	u->vm_up = true;
	for (i = 0; i < UQ_PAGES; i++)
		if ((err = amd_gmc12_vm_map(&u->vm, UQ_VA + i * PAGE_SIZE,
		    u->pg_pa + i * PAGE_SIZE, rwx)) != 0)
			return (err);
	uq_mqd_init(u, p);
	device_printf(m->sc->dev, "uq: process %u: VM base %#jx (%u tables), "
	    "pages pa %#jx, MQD GART %#jx, doorbell %#x\n", p,
	    (uintmax_t)amd_gmc12_vm_pd_addr(&u->vm), u->vm.used,
	    (uintmax_t)u->pg_pa, (uintmax_t)(u->gva + UQ_G_MQD),
	    UQ_DOORBELL(p));
	return (0);
}

/* One IB + RELEASE_MEM (no interrupt) into the user ring, then doorbell. */
static void
uq_kick(struct mes *m, struct uqp *u, u_int p)
{
	volatile uint32_t *ring = (volatile uint32_t *)(u->pg + UQ_RING);
	uint64_t ib = UQ_VA + UQ_IB, fence = UQ_VA + UQ_WB + 0x10;
	u_int n = 0;

	ring[n++] = PKT3(IT_INDIRECT_BUFFER, 2);
	ring[n++] = (uint32_t)ib;
	ring[n++] = (uint32_t)(ib >> 32);
	ring[n++] = IB_VALID | W42_IB_DW;		/* VMID from the HQD */
	ring[n++] = PKT3(IT_RELEASE_MEM, 6);
	ring[n++] = RM_GCR_SEQ | RM_GCR_GL2_WB | RM_CACHE_POLICY(3) |
	    RM_EVENT_TYPE(CACHE_FLUSH_AND_INV_TS_EVENT) | RM_EVENT_INDEX(5);
	ring[n++] = RM_DATA_SEL(1) | RM_INT_SEL(0);
	ring[n++] = (uint32_t)fence;
	ring[n++] = (uint32_t)(fence >> 32);
	ring[n++] = UQ_SEQ + p;
	ring[n++] = 0;
	ring[n++] = 0;
	*(volatile uint64_t *)(u->pg + UQ_WB + 8) = n;
	wmb();
	bus_write_8(m->dbbar, (bus_size_t)UQ_DOORBELL(p) * 4, n);
}

static int
uq_test_run(struct mes *m)
{
	struct atrium_amd_softc *sc = m->sc;
	struct amd_gart *gt = m->gt;
	struct uqp u[UQ_N];
	u_int p, i, us, bad[UQ_N], vmids[UQ_N], active;
	bool fenced[UQ_N], gone = true;
	uint32_t st;
	uint64_t a;
	int err = 0, e;

	memset(u, 0, sizeof(u));
	for (p = 0; p < UQ_N; p++) {
		vmids[p] = 0xff;
		fenced[p] = false;
		bad[p] = W42_OUT_DW;
	}
	if (amd_gmc12_fault_take(gt, &st, &a))
		device_printf(sc->dev, "uq: stale fault %#x at %#jx (cleared)\n",
		    st, (uintmax_t)a);
	for (p = 0; p < UQ_N && err == 0; p++)
		if ((err = uq_setup(m, &u[p], p)) != 0)
			device_printf(sc->dev, "uq: process %u setup failed "
			    "(%d)\n", p, err);
	if (err == 0 && (err = amd_gmc12_flush(gt)) != 0)
		device_printf(sc->dev, "uq: GART flush failed\n");
	for (p = 0; p < UQ_N && err == 0; p++) {
		if ((err = uq_add(m, &u[p], p)) == 0)
			u[p].added = true;
	}
	if (err != 0)
		goto out;
	active = uq_hqd_scan(m, "both added", vmids);
	device_printf(sc->dev, "uq: %u HQD(s) active; process VMIDs %u and "
	    "%u\n", active, vmids[0], vmids[1]);

	for (p = 0; p < UQ_N; p++)
		uq_kick(m, &u[p], p);
	for (us = 0; us < DISP_TIMEOUT_US; us += 10) {
		for (p = 0; p < UQ_N; p++)
			fenced[p] = *(volatile uint32_t *)(u[p].pg + UQ_WB +
			    0x10) == UQ_SEQ + p;
		if (fenced[0] && fenced[1])
			break;
		DELAY(10);
	}
	for (p = 0; p < UQ_N; p++) {
		volatile uint32_t *out = (volatile uint32_t *)(u[p].pg + UQ_OUT);

		bad[p] = vmt_count_not(out, W42_OUT_DW, W42_OUT_VALUE);
		device_printf(sc->dev, "uq: process %u: %s, out %u of %u words "
		    "42 (out[0] %#x), rptr report %#x\n", p, fenced[p] ?
		    "fenced" : "NO FENCE", W42_OUT_DW - bad[p], W42_OUT_DW,
		    out[0], *(volatile uint32_t *)(u[p].pg + UQ_WB));
	}
	/*
	 * MES maps a user queue onto an HQD when it has work (run
	 * T013353Z: right after ADD_QUEUE only process 0 was resident, yet
	 * both ran). The queues stay mapped until REMOVE_QUEUE, so read the
	 * VMIDs again now that both have run.
	 */
	active = uq_hqd_scan(m, "after both ran", vmids);
	device_printf(sc->dev, "uq: %u HQD(s) active; process VMIDs %u and "
	    "%u\n", active, vmids[0], vmids[1]);
	if (amd_gmc12_fault_take(gt, &st, &a)) {
		amd_gmc12_fault_decode(gt, st, "uq");
		device_printf(sc->dev, "uq: fault page %#jx\n", (uintmax_t)a);
		err = EIO;
	}
	if (!fenced[0] || !fenced[1]) {
		mes_report(m, "uq fence timeout");
		(void)uq_hqd_scan(m, "fence timeout", NULL);
		for (p = 0; p < UQ_N; p++)
			if (!fenced[p])
				(void)amd_gfx12_sq_kill_vmid(sc, vmids[p] & 0xf);
		err = ETIMEDOUT;
	} else if (bad[0] != 0 || bad[1] != 0)
		err = EIO;
	else if (vmids[0] == vmids[1] || vmids[0] < 8 || vmids[0] > 15 ||
	    vmids[1] < 8 || vmids[1] > 15) {
		device_printf(sc->dev, "uq: VMIDs %u/%u: want two different "
		    "MES VMIDs (8-15)\n", vmids[0], vmids[1]);
		err = EIO;
	}

out:
	for (p = 0; p < UQ_N; p++)
		if (u[p].added && (e = uq_remove(m, &u[p], p)) != 0) {
			gone = false;
			if (err == 0)
				err = e;
		}
	active = uq_hqd_scan(m, "after REMOVE_QUEUE", NULL);
	if (active > 0) {
		device_printf(sc->dev, "uq: %u HQD(s) still active after "
		    "REMOVE_QUEUE\n", active);
		gone = false;
		if (err == 0)
			err = EIO;
	}
	for (p = 0; p < UQ_N; p++) {
		if (u[p].g == NULL)
			continue;
		for (i = 0; i < UQ_G_SIZE / PAGE_SIZE; i++) {
			uint64_t idx = (u[p].gva - gt->gart_start) / PAGE_SIZE +
			    i;

			bus_write_4(gt->vram, gt->table_off + idx * 8, 0);
			bus_write_4(gt->vram, gt->table_off + idx * 8 + 4, 0);
		}
	}
	(void)amd_gmc12_flush(gt);
	for (p = 0; p < UQ_N; p++) {
		if (!gone)
			break;
		if (u[p].vm_up)
			amd_gmc12_vm_fini(&u[p].vm);
		if (u[p].pg != NULL)
			contigfree(u[p].pg, UQ_PAGES * PAGE_SIZE, M_DEVBUF);
		if (u[p].g != NULL)
			contigfree(u[p].g, UQ_G_SIZE, M_DEVBUF);
	}
	if (!gone)
		device_printf(sc->dev, "uq: queues not known gone; their pages "
		    "are leaked, not freed\n");
	device_printf(sc->dev, "uq: stage 5c-1: %s\n", err == 0 ?
	    "PASS (two MES user queues, own VMs and VMIDs, same VAs, own "
	    "pages)" : "FAILED");
	return (err);
}

/*
 * Stage 5c-2b: MES user queues for client queue fds (si_client.c). One
 * compute queue per struct amd_uq, as 5c-1 proved them, generalised:
 *
 *   slot     0..AMD_UQ_SLOTS-1, chosen by the caller; it fixes the
 *            doorbell (5c-1's userqueue range, UQ_DOORBELL(slot)), the GART
 *            block (MQD, process and gang contexts, the writeback page's
 *            alias for MES) and the process id MES sees.
 *   memory   ring, writeback (rptr 0, wptr 8, fence 0x10) and EOP buffer:
 *            three kernel-allocated pages mapped into the client's VM at
 *            `va` (a kernel-reserved range above the BO window, never handed
 *            to a BO). The ring is mapped read-only (and executable: the
 *            CPF fetch needs it), so a client shader cannot rewrite it.
 *   process  one MES process (and gang) per queue, so two queues of one VM
 *            are two MES processes on the same page tables. MES picks the
 *            HQD and the VMID (8-15) when the queue has work.
 *
 * The fence is in the client's own VM, so a client can lie about its own
 * queue's progress and nothing else's. Callers serialise per queue.
 */
#define UQ2_GART_VA		0x00080000ull	/* after the kq's block */
#define UQ2_PAGES		3u
#define UQ2_RING		0x0000u
#define UQ2_WB			0x1000u
#define UQ2_EOP			0x2000u
#define UQ2_PID(slot)		(0x200u + (slot))
#define UQ2_PKT_DW		12u
#define UQ2_KILL_WAIT_US	200000

struct amd_uq {
	struct mes *m;
	struct amd_vm12 *vm;
	u_int	 slot;
	uint32_t db;
	uint64_t va;		/* ring/WB/EOP in the client's VM */
	uint8_t	*pg;		/* their KVA */
	vm_paddr_t pg_pa;
	uint8_t	*g;		/* MQD, process ctx, gang ctx */
	vm_paddr_t g_pa;
	uint64_t gva;		/* GART VA of the block */
	uint64_t wptr;		/* dwords emitted */
	uint32_t seq;		/* last fence value emitted */
	bool	 mapped_vm, added;
	bool	 user;		/* user mode: the client writes ring and wptr
				 * and rings its own doorbell page; pg is the
				 * caller's (rs/uqueue.rs RamPages) */
};

static bool
uq2_seq_passed(uint32_t fence, uint32_t seq)
{
	return ((int32_t)(fence - seq) >= 0);
}

/*
 * A user-mode queue's writeback page: the CP's rptr report (+0, dwords
 * into the ring), the client's wptr (+8), the fence its RELEASE_MEMs
 * write (+0x10) and the last fence value it emitted (+0x18). It is busy
 * while the CP has not fetched up to the wptr or the fence is behind what
 * was emitted; its progress is rptr and fence. The CP's rptr runs ahead
 * of execution (a faulted IB showed the rptr past its RELEASE_MEM), so
 * only the fence says work is done. The client can lie about all of it,
 * which only fools the clock on its own queue.
 */
#define UQ2_WB_EMITTED		0x18u

static bool
uq2_user_busy(struct amd_uq *q, uint32_t *rptr, uint32_t *fence)
{
	volatile uint8_t *wb = q->pg + UQ2_WB;
	uint64_t wptr = *(volatile uint64_t *)(wb + 8);
	uint32_t emitted = *(volatile uint32_t *)(wb + UQ2_WB_EMITTED);

	*rptr = *(volatile uint32_t *)wb & (RING_DW - 1);
	*fence = *(volatile uint32_t *)(wb + 0x10);
	return ((uint32_t)(wptr & (RING_DW - 1)) != *rptr ||
	    !uq2_seq_passed(*fence, emitted));
}

/* Nothing left to run: the last fence landed (kernel mode; user mode: as
 * far as the client said it emitted, and the CP fetched it all). */
static bool
uq2_done(struct amd_uq *q)
{
	uint32_t rptr, fence;

	if (q->user)
		return (!uq2_user_busy(q, &rptr, &fence));
	return (uq2_seq_passed(amd_mes12_uq_fence(q), q->seq));
}

bool
amd_mes12_uq_user_busy(struct amd_uq *q, uint32_t *rptr, uint32_t *fence)
{
	return (uq2_user_busy(q, rptr, fence));
}

/* The GPU VA (in the client's VM) of a queue's fence. */
uint64_t
amd_mes12_uq_fence_va(struct amd_uq *q)
{
	return (q->va + UQ2_WB + 0x10);
}

/* The active MEC1 HQD (any pipe and queue) whose doorbell is `db`. */
static bool
uq2_hqd_at(struct mes *m, uint32_t db, u_int *vmid, u_int *pipep, u_int *qp)
{
	const struct mes_gen *g = m->g;
	u_int pipe, q;

	for (pipe = 0; pipe < 4; pipe++)
		for (q = 0; q < 8; q++) {
			grbm_select_q(m, 1, pipe, q);
			if ((rd(m, g->hqd_active) & 1) == 0)
				continue;
			if (((rd(m, g->hqd_doorbell) >> 2) & 0x03ffffff) != db)
				continue;
			*vmid = rd(m, g->hqd_vmid) & 0xf;
			if (pipep != NULL)
				*pipep = pipe;
			if (qp != NULL)
				*qp = q;
			grbm_select_q(m, 0, 0, 0);
			return (true);
		}
	grbm_select_q(m, 0, 0, 0);
	return (false);
}

static bool
uq2_hqd_find(struct mes *m, uint32_t db, u_int *vmid)
{
	return (uq2_hqd_at(m, db, vmid, NULL, NULL));
}

/* The VMID MES runs the queue under now; 0 if it is not on an HQD. */
u_int
amd_mes12_uq_hqd_vmid(struct amd_uq *q)
{
	u_int vmid = 0;

	return (uq2_hqd_find(q->m, q->db, &vmid) ? vmid : 0);
}

/*
 * A wedged queue (a CPF fault, waves that do not die) MES cannot preempt:
 * reset its HQD directly, as amdgpu's legacy queue reset does
 * (CP_HQD_DEQUEUE_REQUEST = 2, reset), and wait for it to go inactive.
 */
static bool
uq2_hqd_reset(struct amd_uq *q)
{
	struct mes *m = q->m;
	const struct mes_gen *g = m->g;
	u_int vmid, pipe, qi, us = 0;
	bool idle;

	if (!uq2_hqd_at(m, q->db, &vmid, &pipe, &qi))
		return (true);
	grbm_select_q(m, 1, pipe, qi);
	wr(m, g->hqd_dequeue_req, 2);
	/* amdgpu also resets the queue's SPI state (box runs a0597b2d,
	 * 05fdaebf, 955528b9: without it a CP-faulted HQD stayed active). */
	wr(m, g->spi_compute_queue_reset, 1);
	for (; us < 100000 && (rd(m, g->hqd_active) & 1); us += 10)
		DELAY(10);
	wr(m, g->hqd_dequeue_req, 0);
	idle = (rd(m, g->hqd_active) & 1) == 0;
	grbm_select_q(m, 0, 0, 0);
	device_printf(m->sc->dev, "uq%u: HQD me1 pipe %u queue %u (VMID %u) "
	    "reset: %s after %u us\n", q->slot, pipe, qi, vmid,
	    idle ? "inactive" : "STILL ACTIVE", us);
	return (idle);
}

/* EOP interrupts from every MEC1 pipe MES may place a user queue on. */
void
amd_mes12_uq_ints(struct mes *m, bool on, uint32_t *saved)
{
	u_int pipe;

	for (pipe = 0; pipe < 4; pipe++) {
		grbm_select(m, 1, pipe);
		if (on) {
			saved[pipe] = rd(m, m->g->cpc_int_cntl);
			wr(m, m->g->cpc_int_cntl, saved[pipe] |
			    TIME_STAMP_INT_ENABLE);
		} else
			wr(m, m->g->cpc_int_cntl, saved[pipe]);
	}
	grbm_select(m, 0, 0);
}

static void
uq2_unmap_vm(struct amd_uq *q, u_int kvmid)
{
	u_int i, vmid;

	if (!q->mapped_vm)
		return;
	for (i = 0; i < UQ2_PAGES; i++)
		(void)amd_gmc12_vm_unmap(q->vm, q->va + i * PAGE_SIZE);
	/* The kernel's VMID for the VM, and every MES VMID: any of them
	 * may hold a translation of these pages. */
	(void)amd_gmc12_vm_flush(q->m->gt, kvmid);
	for (vmid = 8; vmid < 16; vmid++)
		(void)amd_gmc12_vm_flush(q->m->gt, vmid);
	q->mapped_vm = false;
}

static void
uq2_free(struct amd_uq *q, u_int kvmid)
{
	uq2_unmap_vm(q, kvmid);
	gart_unmap_blk(q->m->gt, q->gva, UQ_G_SIZE);
	if (!q->user)
		contigfree(q->pg, UQ2_PAGES * PAGE_SIZE, M_DEVBUF);
	contigfree(q->g, UQ_G_SIZE, M_DEVBUF);
	free(q, M_DEVBUF);
}

/*
 * `upg` (UQ2_PAGES zeroed, contiguous pages at `upa`, the caller's until
 * the queue is destroyed or freed lost) makes a user-mode queue: its
 * doorbell is its own doorbell BAR page (AMD_UQ_DOORBELL_USER), which the
 * client maps with ring and writeback page; the kernel never writes the
 * ring. NULL: a kernel-mode queue on pages of its own.
 */
int
amd_mes12_uq_create(struct mes *m, struct amd_vm12 *vm, u_int kvmid,
    u_int slot, uint64_t va, uint32_t prio, uint8_t *upg, vm_paddr_t upa,
    struct amd_uq **out)
{
	struct amd_gart *gt = m->gt;
	struct amd_uq *q;
	u_int i;
	int err;

	*out = NULL;
	if (slot >= AMD_UQ_SLOTS || (va & PAGE_MASK) != 0)
		return (EINVAL);
	if (upg != NULL && !m->udb_registered)
		return (ENXIO);
	q = malloc(sizeof(*q), M_DEVBUF, M_WAITOK | M_ZERO);
	q->m = m;
	q->vm = vm;
	q->slot = slot;
	q->user = upg != NULL;
	q->db = q->user ? AMD_UQ_DOORBELL_USER(slot) : AMD_UQ_DOORBELL(slot);
	q->va = va;
	q->gva = gt->gart_start + UQ2_GART_VA + (uint64_t)slot * UQ_G_SIZE;
	if (q->gva + UQ_G_SIZE > gt->gart_end + 1) {
		free(q, M_DEVBUF);
		return (EINVAL);
	}
	if (q->user) {
		q->pg = upg;
		q->pg_pa = upa;
	} else {
		q->pg = contigmalloc(UQ2_PAGES * PAGE_SIZE, M_DEVBUF,
		    M_WAITOK | M_ZERO, 0, ~(vm_paddr_t)0, PAGE_SIZE, 0);
		q->pg_pa = vtophys(q->pg);
	}
	q->g = contigmalloc(UQ_G_SIZE, M_DEVBUF, M_WAITOK | M_ZERO, 0,
	    ~(vm_paddr_t)0, PAGE_SIZE, 0);
	q->g_pa = vtophys(q->g);

	/* GART: MQD, process ctx, gang ctx, and the writeback alias. */
	for (i = 0; i < 3; i++)
		(void)amd_gmc12_gart_map(gt, q->gva + i * PAGE_SIZE,
		    q->g_pa + i * PAGE_SIZE);
	(void)amd_gmc12_gart_map(gt, q->gva + UQ_G_WB, q->pg_pa + UQ2_WB);
	if ((err = amd_gmc12_flush(gt)) != 0)
		goto fail;

	/*
	 * The client's VM: ring read-only and executable (the CPF fetches the
	 * ring as an execute access; box run 2026-11-13: a ring PTE without it
	 * faulted, PERMISSION_FAULTS 0x8). Writeback and EOP are read-write;
	 * with isolation.gpu_wx bit 2 they lose X. Box run 05fdaebf: a queue
	 * without X there was lost to a fault on its first job and MES then
	 * refused to remove it (mode 1 reset), so the default keeps X.
	 */
	q->mapped_vm = true;
	for (i = 0; i < UQ2_PAGES; i++)
		if ((err = amd_gmc12_vm_map(vm, va + i * PAGE_SIZE,
		    q->pg_pa + i * PAGE_SIZE, i * PAGE_SIZE == UQ2_RING ?
		    AMD_VM_READ | AMD_VM_EXEC :
		    AMD_VM_READ | AMD_VM_WRITE |
		    ((m->sc->gpu_wx & AMD_WX_UQ) != 0 ? 0 : AMD_VM_EXEC))) != 0)
			goto fail;
	if ((err = amd_gmc12_vm_flush(gt, kvmid)) != 0)
		goto fail;

	uq_mqd_build(q->g + UQ_G_MQD, q->gva + UQ_G_MQD, va + UQ2_RING,
	    va + UQ2_EOP, va + UQ2_WB, q->db);
	if ((err = uq_add_raw(m, vm, q->gva, UQ2_PID(slot), q->db,
	    prio)) != 0)
		goto fail;
	q->added = true;
	device_printf(m->sc->dev, "uq%u: added%s: VM base %#jx, queue pages at "
	    "VA %#jx (pa %#jx), MQD GART %#jx, doorbell %#x\n", slot,
	    q->user ? " (user mode)" : "",
	    (uintmax_t)amd_gmc12_vm_pd_addr(vm), (uintmax_t)va,
	    (uintmax_t)q->pg_pa, (uintmax_t)(q->gva + UQ_G_MQD), q->db);
	*out = q;
	return (0);
fail:
	/* Not added: MES never saw the block. */
	uq2_free(q, kvmid);
	return (err);
}

uint32_t
amd_mes12_uq_fence(struct amd_uq *q)
{
	return (*(volatile uint32_t *)(q->pg + UQ2_WB + 0x10));
}

uint32_t
amd_mes12_uq_doorbell(struct amd_uq *q)
{
	return (q->db);
}

/* A user-mode queue's doorbell page (registered fictitious); else 0. */
vm_paddr_t
amd_mes12_uq_doorbell_pa(struct amd_uq *q)
{
	if (!q->user)
		return (0);
	return (rman_get_start(q->m->dbbar) + (vm_paddr_t)q->db * 4);
}

uint32_t
amd_mes12_uq_seq(struct amd_uq *q)
{
	return (q->seq);
}

/*
 * Emit one job: IB at `ib` (`ib_dw` dwords; the VMID is the HQD's), then
 * the fence and an EOP interrupt; write wptr and ring the doorbell. *seq
 * is the job's fence value. EBUSY if a ring's worth is in flight. Does
 * not sleep (the caller holds sc->lock).
 */
int
amd_mes12_uq_submit(struct amd_uq *q, uint64_t ib, uint32_t ib_dw,
    uint32_t *seq)
{
	volatile uint32_t *ring = (volatile uint32_t *)(q->pg + UQ2_RING);
	uint64_t fence = q->va + UQ2_WB + 0x10;
	uint32_t pkt[UQ2_PKT_DW], s, inflight;
	u_int i, n = 0;

	if (!q->added)
		return (ENXIO);
	if (q->user)
		return (EOPNOTSUPP);		/* the client's ring */
	if (ib_dw == 0 || ib_dw > 0xfffffu || (ib & 3) != 0)
		return (EINVAL);
	/* The fence is the client's to write: a lie only makes its own
	 * ring look emptier, and the ring is ours (read-only to it). */
	inflight = q->seq - amd_mes12_uq_fence(q);
	if (inflight > RING_DW / UQ2_PKT_DW ||
	    (inflight + 1) * UQ2_PKT_DW >= RING_DW)
		return (EBUSY);
	s = q->seq + 1;
	if (s == 0)
		s = 1;
	pkt[n++] = PKT3(IT_INDIRECT_BUFFER, 2);
	pkt[n++] = (uint32_t)ib;
	pkt[n++] = (uint32_t)(ib >> 32);
	pkt[n++] = IB_VALID | ib_dw;			/* VMID from the HQD */
	pkt[n++] = PKT3(IT_RELEASE_MEM, 6);
	pkt[n++] = RM_GCR_SEQ | RM_GCR_GL2_WB | RM_CACHE_POLICY(3) |
	    RM_EVENT_TYPE(CACHE_FLUSH_AND_INV_TS_EVENT) | RM_EVENT_INDEX(5);
	pkt[n++] = RM_DATA_SEL(1) | RM_INT_SEL(2);
	pkt[n++] = (uint32_t)fence;
	pkt[n++] = (uint32_t)(fence >> 32);
	pkt[n++] = s;
	pkt[n++] = 0;
	/* INT_CTXID: the EOP IV's src_data[0] (gfx_v11_0_eop_irq's
	 * AMDGPU_FENCE_MES_QUEUE_FLAG scheme); names this queue. */
	pkt[n++] = AMD_UQ_CTXID(q->db);
	for (i = 0; i < n; i++)
		ring[(q->wptr + i) & (RING_DW - 1)] = pkt[i];
	q->wptr += n;
	q->seq = s;
	*(volatile uint64_t *)(q->pg + UQ2_WB + 8) = q->wptr;
	wmb();
	bus_write_8(q->m->dbbar, (bus_size_t)q->db * 4, q->wptr);
	*seq = s;
	return (0);
}

/*
 * The free rule (spec §4.5.1 step 4): preempt the queue's gang once,
 * SUSPEND until MES writes the suspend fence (kernel memory, never the
 * client's), then RESUME. Whatever the gang had in flight when its VM's
 * PTEs were cleared and flushed has then left the GPU, so a page only
 * those PTEs reached can be freed. mes_v12 SUSPEND (MESAPI__SUSPEND):
 * gang context at 8, fence address 16, value 24, api_status 32; RESUME:
 * gang context 8, api_status 16. 0: preempted and running again.
 */
#define SU_GANG_CTX		8
#define SU_FENCE_ADDR		16
#define SU_FENCE_VALUE		24
#define SU_API_STATUS		32
#define RS_GANG_CTX		8
#define RS_API_STATUS		16
#define UQ2_SUSPEND_WAIT_US	2100000

int
amd_mes12_uq_preempt(struct amd_uq *q)
{
	struct mes *m = q->m;
	uint32_t pkt[API_DW], v;
	uint8_t *b = (uint8_t *)pkt;
	bool suspended;
	int err, us;

	if (!q->added)
		return (0);			/* off MES: nothing in flight */
	v = ++m->suspend_seq;
	memset(pkt, 0, sizeof(pkt));
	pkt[0] = HDR_SUSPEND;
	put64(b, SU_GANG_CTX, q->gva + UQ_G_GANG);
	put64(b, SU_FENCE_ADDR, m->va + BLK_WB(0) + WB_SUSPEND_FENCE);
	put32(b, SU_FENCE_VALUE, v);
	if ((err = mes_api(m, 0, pkt, SU_API_STATUS,
	    "SUSPEND (user gang)")) != 0)
		return (err);
	for (us = 0; us < UQ2_SUSPEND_WAIT_US &&
	    (uint32_t)*p64(m, BLK_WB(0) + WB_SUSPEND_FENCE) != v; us += 10)
		DELAY(10);
	suspended = (uint32_t)*p64(m, BLK_WB(0) + WB_SUSPEND_FENCE) == v;
	/* Resume whatever happened: a queue left suspended would hang. */
	memset(pkt, 0, sizeof(pkt));
	pkt[0] = HDR_RESUME;
	put64(b, RS_GANG_CTX, q->gva + UQ_G_GANG);
	err = mes_api(m, 0, pkt, RS_API_STATUS, "RESUME (user gang)");
	device_printf(m->sc->dev, "uq%u: preempted for a free: suspend fence "
	    "%s after %d us, resume %s\n", q->slot, suspended ? "landed" :
	    "MISSING", us, err == 0 ? "ok" : "FAILED");
	if (!suspended)
		return (ETIMEDOUT);
	return (err);
}

void
amd_mes12_uq_report(struct amd_uq *q, const char *when)
{
	volatile uint32_t *wb = (volatile uint32_t *)(q->pg + UQ2_WB);
	u_int vmid = 0;
	bool on;

	on = uq2_hqd_find(q->m, q->db, &vmid);
	device_printf(q->m->sc->dev, "uq%u: %s: seq %u, fence %u, wptr %ju%s, "
	    "rptr report %#x; HQD %s (VMID %u)\n", q->slot, when, q->seq,
	    wb[4], q->user ? (uintmax_t)*(volatile uint64_t *)(wb + 2) :
	    (uintmax_t)q->wptr, q->user ? " (client's)" : "", wb[0],
	    on ? "active" : "not mapped", vmid);
}

/*
 * The card was reset and bus mastering is off (amd_si_lose): nothing can
 * reach the queue's memory, so free it without touching the hardware.
 */
void
amd_mes12_uq_free_lost(struct amd_uq *q)
{
	device_printf(q->m->sc->dev, "uq%u: freed after the GPU reset\n",
	    q->slot);
	if (!q->user)
		contigfree(q->pg, UQ2_PAGES * PAGE_SIZE, M_DEVBUF);
	contigfree(q->g, UQ_G_SIZE, M_DEVBUF);
	free(q, M_DEVBUF);
}

/*
 * Evict: take the queue off MES, keeping its memory (a lost queue: its
 * close frees it later). Destroy: evict and free. If its last job has not fenced,
 * first kill the waves of the VMID MES runs it under and give the fence a
 * moment. 0: removed, nothing of it is mapped, its memory is freed.
 * EBUSY: MES did not let go of it; its memory is leaked (never freed under
 * the GPU), and the caller must keep the VM's page tables alive too.
 */
int
amd_mes12_uq_evict(struct amd_uq *q)
{
	struct mes *m = q->m;
	u_int vmid, us;
	int err;

	if (q->added && !uq2_done(q)) {
		amd_mes12_uq_report(q, "destroy: work in flight");
		if (uq2_hqd_find(m, q->db, &vmid) && vmid >= 8) {
			err = amd_gfx12_sq_kill_vmid(m->sc, vmid);
			for (us = 0; us < UQ2_KILL_WAIT_US && !uq2_done(q);
			    us += 100)
				DELAY(100);
			device_printf(m->sc->dev, "uq%u: VMID %u's waves "
			    "killed (%d); fence %s after %u us\n", q->slot,
			    vmid, err, uq2_done(q) ? "landed" : "still behind",
			    us);
		}
	}
	if (q->added && !uq2_done(q))
		(void)uq2_hqd_reset(q);
	if (q->added) {
		err = uq_remove_raw(m, q->gva, q->db);
		if (err != 0 && uq2_hqd_reset(q)) {
			device_printf(m->sc->dev, "uq%u: REMOVE_QUEUE refused "
			    "(%d); HQD reset, trying once more\n", q->slot, err);
			err = uq_remove_raw(m, q->gva, q->db);
		}
		if (err != 0) {
			/* Still refused: MES's own queue reset, then once
			 * more. Only if that fails too does the caller reset
			 * the GPU. */
			int rerr = uq_reset_raw(m, q->gva, q->db);

			(void)uq2_hqd_reset(q);
			err = uq_remove_raw(m, q->gva, q->db);
			device_printf(m->sc->dev, "uq%u: after MES RESET (%d): "
			    "REMOVE_QUEUE %s\n", q->slot, rerr, err == 0 ?
			    "ok" : "still refused");
		}
		if (err != 0 || uq2_hqd_find(m, q->db, &vmid)) {
			amd_mes12_uq_report(q, "REMOVE_QUEUE failed");
			device_printf(m->sc->dev, "uq%u: still mapped after "
			    "REMOVE_QUEUE (%d); its memory and VM are "
			    "leaked\n", q->slot, err);
			return (EBUSY);
		}
		q->added = false;
		device_printf(m->sc->dev, "uq%u: removed (seq %u, fence %u)\n",
		    q->slot, q->seq, amd_mes12_uq_fence(q));
	}
	return (0);
}

int
amd_mes12_uq_destroy(struct amd_uq *q, u_int kvmid)
{
	int err;

	if ((err = amd_mes12_uq_evict(q)) != 0)
		return (err);
	uq2_free(q, kvmid);
	return (0);
}

static void mes_free(struct mes *m);

static int
mes_fw_start(struct mes *m)
{
	struct atrium_amd_softc *sc = m->sc;
	const struct firmware *fw;
	char name[64];
	int r;

	snprintf(name, sizeof(name), "amdgpu/gc_%u_%u_%u_uni_mes.bin",
	    sc->ip.gc.major, sc->ip.gc.minor, sc->ip.gc.rev);
	fw = firmware_get_flags(name, FIRMWARE_GET_NOWARN);
	if (fw == NULL) {
		device_printf(sc->dev, "mes: no /boot/firmware/%s\n", name);
		return (ENOENT);
	}
	r = atrium_amd_mes_start(fw->data, fw->datasize, &m->uc_start);
	firmware_put(fw, FIRMWARE_UNLOAD);
	if (r != 0) {
		device_printf(sc->dev, "mes: %s header refused\n", name);
		return (EINVAL);
	}
	return (0);
}

static int
mes_run(struct mes *m)
{
	const struct mes_gen *g = m->g;
	struct atrium_amd_softc *sc = m->sc;
	uint32_t d;
	int err, p, i, attempt;

	/* NBIF: doorbell aperture on, GC doorbell routing (gc_doorbell_init). */
	d = amd_mmio_read32(sc, nbreg(sc, g->nbif_db_aper_en));
	amd_mmio_write32(sc, nbreg(sc, g->nbif_db_aper_en), d | 1);
	amd_mmio_write32(sc, nbreg(sc, g->nbif_db_entry0), 0x30000007);
	amd_mmio_write32(sc, nbreg(sc, g->nbif_db_entry3), 0x3000000d);

	/* KIQ = me 3, pipe 1, queue 0 (mes_v12_0_kiq_setting). */
	d = rd(m, g->rlc_cp_schedulers) & 0xffffff00u;
	wr(m, g->rlc_cp_schedulers, d | (3u << 5) | (1u << 3) | 0x80);

	/* MES pipes: EOP interrupt on API completion (TIME_STAMP_INT). */
	for (p = 0; p < 2; p++) {
		grbm_select(m, 3, p);
		m->cpc_int_cntl[p] = rd(m, g->cpc_int_cntl);
		wr(m, g->cpc_int_cntl, m->cpc_int_cntl[p] |
		    TIME_STAMP_INT_ENABLE);
	}
	grbm_select(m, 0, 0);

	mes_report(m, "before");
	mes_placement(m, "before start");
	amd_gmc12_fault_report(m->gt, "before MES");
	amd_gfx12_pm_report(sc, "before MES");
	if ((err = cp_resume(m)) != 0)
		return (err);
	/*
	 * Both pipes must be running the firmware before any queue is made
	 * active on them. Box run f4265f28: pipe 1 was still at instr 0x1,
	 * GP3 0 500 us after the start; its HQD was activated anyway, the
	 * wptr post wedged CPF and CPC, and the dequeue timed out. Wait for
	 * each pipe's firmware version (GP3) to appear, up to 100 ms.
	 *
	 * Box runs 20:36-21:04: the start is intermittent; a pipe that does
	 * not start faults in the GFX hub at 0x3f01NN000, i.e. its own data
	 * addresses (0xfffffffff01NN000) truncated to 34 bits, as if its
	 * local data aperture were not in effect. Try the start up to three
	 * times, halting (pipe reset, icache invalidate) between attempts,
	 * and log each attempt; still refuse if none takes.
	 */
	for (attempt = 1; attempt <= 3; attempt++) {
		bool up = true;

		for (p = 0; p < 2; p++) {
			grbm_select(m, 3, p);
			wr(m, g->mes_gp3_lo, 0);
		}
		grbm_select(m, 0, 0);
		mes_enable(m, true);
		for (p = 0; p < 2; p++) {
			uint32_t gp3 = 0;
			int us;

			grbm_select(m, 3, p);
			for (us = 0; us < 100000; us += 100) {
				if ((gp3 = rd(m, g->mes_gp3_lo)) != 0)
					break;
				DELAY(100);
			}
			grbm_select(m, 0, 0);
			device_printf(sc->dev, "mes: attempt %d: pipe %d %s "
			    "(GP3 %#x) after %d us\n", attempt, p, gp3 != 0 ?
			    "firmware up" : "NOT started", gp3, us);
			if (gp3 == 0)
				up = false;
		}
		if (up)
			break;
		mes_report(m, "pipe not started");
		mes_placement(m, "pipe not started");
		amd_gmc12_fault_report(m->gt, "pipe not started");
		mes_enable(m, false);
		DELAY(1000);
	}
	if (attempt > 3) {
		device_printf(sc->dev, "mes: MES did not start in 3 attempts; "
		    "no queues made active\n");
		return (ENXIO);
	}
	device_printf(sc->dev, "mes: started on attempt %d of 3%s\n", attempt,
	    attempt > 1 ? " (retry used)" : "");
	mes_report(m, "started");

	/* Pipe 1 (KIQ): MQD and HQD by register. */
	mqd_init(m, 1);
	queue_init_register(m, 1);
	grbm_select(m, 3, 1);
	m->version[1] = rd(m, g->mes_gp3_lo);
	grbm_select(m, 0, 0);
	device_printf(sc->dev, "mes: version (pipe 1) %#x\n", m->version[1]);
	if ((err = set_hw_resources(m, 1)) != 0)
		return (err);
	(void)set_hw_resources_1(m, 1);

	/* Pipe 0 (scheduler): unmapped doorbells, ADD_QUEUE via pipe 1. */
	d = rd(m, g->unmapped_doorbell);
	wr(m, g->unmapped_doorbell, (d & ~0x1f00u) | (0xdu << 8) | 1);
	mqd_init(m, 0);
	if ((err = map_sched_queue(m)) != 0)
		return (err);
	m->sched_mapped = true;
	grbm_select(m, 3, 0);
	m->version[0] = rd(m, g->mes_gp3_lo);
	grbm_select(m, 0, 0);
	device_printf(sc->dev, "mes: version (pipe 0) %#x\n", m->version[0]);
	if ((err = set_hw_resources(m, 0)) != 0)
		return (err);
	if ((m->version[0] & 0xfff) >= 0x4b)
		(void)set_hw_resources_1(m, 0);
	/* Aggregated doorbells, low..realtime (init_aggregated_doorbell). */
	for (i = 0; i < 5; i++) {
		d = rd(m, g->mes_db_cntl1 + i);
		d &= ~(0x0ffffffcu | (1u << 30) | (1u << 31));
		d |= ((g->db_aggregated + 2 * i) << 2) | (1u << 30);
		wr(m, g->mes_db_cntl1 + i, d);
	}
	wr(m, g->hqd_gfx_control, 1u << 15);	/* DB_UPDATED_MSG_EN */
	return (query_status(m, 0));
}

/*
 * MES up and answering (stage 4d's bring-up), left running: the block in
 * the GART, the doorbell BAR, both pipes, the scheduler queue. `kq`: keep
 * MEC1 pipe 0 queue 0 for a kernel compute queue (4e, the attached driver).
 * GC powered, hubs and GART up (4a), GFX set up with the CP halted (4c).
 * On error everything this did is undone and *out is NULL.
 */
int
amd_mes12_up(struct atrium_amd_softc *sc, struct amd_gart *gt, bool kq,
    struct mes **out)
{
	struct mes *m;
	uint32_t i;
	int err, scratch;

	*out = NULL;
	m = malloc(sizeof(*m), M_DEVBUF, M_WAITOK | M_ZERO);
	m->sc = sc;
	m->gt = gt;
	m->dispatch = kq;
	m->g = mes_gen_find(sc);
	if (m->g == NULL) {
		device_printf(sc->dev, "mes: no table for GC %u.%u.%u; not "
		    "touching it\n", sc->ip.gc.major, sc->ip.gc.minor,
		    sc->ip.gc.rev);
		free(m, M_DEVBUF);
		return (ENOTSUP);
	}
	if (!gt->enabled) {
		free(m, M_DEVBUF);
		return (ENXIO);
	}
	if ((err = mes_fw_start(m)) != 0) {
		free(m, M_DEVBUF);
		return (err);
	}
	m->r[0].pipe = 0;
	m->r[0].db = m->g->db_pipe0;
	m->r[1].pipe = 1;
	m->r[1].db = m->g->db_pipe1;

	m->dbrid = PCIR_BAR(2);
	m->dbbar = bus_alloc_resource_any(sc->dev, SYS_RES_MEMORY, &m->dbrid,
	    RF_ACTIVE);
	if (m->dbbar == NULL || rman_get_size(m->dbbar) <
	    (bus_size_t)(m->g->db_aggregated + 16) * 4) {
		device_printf(sc->dev, "mes: doorbell BAR unusable\n");
		if (m->dbbar != NULL)
			bus_release_resource(sc->dev, SYS_RES_MEMORY, m->dbrid,
			    m->dbbar);
		free(m, M_DEVBUF);
		return (ENXIO);
	}
	/*
	 * The user-mode queues' doorbell pages (one BAR page each, after the
	 * kernel's page 0 and MES's aggregated doorbells on page 1) as
	 * fictitious pages, so a queue fd's pager (rs/uqueue.rs) can map
	 * exactly one of them into its client and take it back.
	 */
	if (rman_get_size(m->dbbar) >= (bus_size_t)AMD_UQ_DOORBELL_USER(
	    AMD_UQ_SLOTS) * 4 && vm_phys_fictitious_reg_range(
	    rman_get_start(m->dbbar) + AMD_UQ_DOORBELL_USER(0) * 4,
	    rman_get_start(m->dbbar) + AMD_UQ_DOORBELL_USER(AMD_UQ_SLOTS) * 4,
	    VM_MEMATTR_DEVICE) == 0)
		m->udb_registered = true;
	m->blk = contigmalloc(BLK_SIZE, M_DEVBUF, M_WAITOK | M_ZERO, 0,
	    ~(vm_paddr_t)0, PAGE_SIZE, 0);
	m->blk_pa = vtophys(m->blk);
	m->va = gt->gart_start + MES_GART_VA;
	for (i = 0; i < BLK_SIZE / PAGE_SIZE; i++)
		(void)amd_gmc12_gart_map(gt, m->va + i * PAGE_SIZE,
		    m->blk_pa + i * PAGE_SIZE);
	scratch = 1;
	TUNABLE_INT_FETCH("hw.atrium_gpu.mes_mscratch", &scratch);
	if (scratch) {
		m->scratch = contigmalloc(2 * MES_SCRATCH_SIZE, M_DEVBUF,
		    M_WAITOK | M_ZERO, 0, ~(vm_paddr_t)0, PAGE_SIZE, 0);
		m->scratch_pa = vtophys(m->scratch);
		m->scratch_va = gt->gart_start + MES_SCRATCH_VA;
		for (i = 0; i < 2 * MES_SCRATCH_SIZE / PAGE_SIZE; i++)
			(void)amd_gmc12_gart_map(gt, m->scratch_va +
			    i * PAGE_SIZE, m->scratch_pa + i * PAGE_SIZE);
		device_printf(sc->dev, "mes: MSCRATCH 2 x %u KiB at GART "
		    "%#jx, zeroed before every start\n", MES_SCRATCH_SIZE >> 10,
		    (uintmax_t)m->scratch_va);
	}
	if ((err = amd_gmc12_flush(gt)) != 0) {
		mes_free(m);
		return (err);
	}
	device_printf(sc->dev, "mes: %s, start %#jx, block at GART %#jx (pa "
	    "%#jx), doorbell BAR %ju KiB\n", m->g->name, (uintmax_t)m->uc_start,
	    (uintmax_t)m->va, (uintmax_t)m->blk_pa,
	    (uintmax_t)(rman_get_size(m->dbbar) >> 10));

	/* MES reads its rings and writes fences in system memory. */
	m->pci_cmd = pci_read_config(sc->dev, PCIR_COMMAND, 2);
	pci_enable_busmaster(sc->dev);
	err = mes_run(m);
	mes_report(m, err == 0 ? "answered" : "failed");
	if (err != 0) {
		amd_mes12_down(m);
		return (err);
	}
	*out = m;
	return (0);
}

/* The 4e dispatch (write_42 through a kernel compute queue), MES up. */
int
amd_mes12_selftest(struct mes *m)
{
	if (!m->dispatch)
		return (ENOTSUP);
	return (dispatch_run(m));
}

/* Stage 5b: VMID 1 through its own page table (vm_test_run), MES up. */
int
amd_mes12_vmtest(struct mes *m)
{
	if (!m->dispatch)
		return (ENOTSUP);
	return (vm_test_run(m));
}

/* Stage 5c-1: two MES user queues in their own VMs (uq_test_run). */
int
amd_mes12_uqtest(struct mes *m)
{
	if (!m->dispatch)
		return (ENOTSUP);
	return (uq_test_run(m));
}

/* Unmap the block and free it, release the doorbell BAR. */
static void
mes_free(struct mes *m)
{
	struct amd_gart *gt = m->gt;
	uint32_t i;

	for (i = 0; i < BLK_SIZE / PAGE_SIZE; i++) {
		uint64_t idx = (m->va - gt->gart_start) / PAGE_SIZE + i;

		bus_write_4(gt->vram, gt->table_off + idx * 8, 0);
		bus_write_4(gt->vram, gt->table_off + idx * 8 + 4, 0);
	}
	if (m->scratch != NULL) {
		for (i = 0; i < 2 * MES_SCRATCH_SIZE / PAGE_SIZE; i++) {
			uint64_t idx = (m->scratch_va - gt->gart_start) /
			    PAGE_SIZE + i;

			bus_write_4(gt->vram, gt->table_off + idx * 8, 0);
			bus_write_4(gt->vram, gt->table_off + idx * 8 + 4, 0);
		}
	}
	(void)amd_gmc12_flush(gt);
	contigfree(m->blk, BLK_SIZE, M_DEVBUF);
	if (m->scratch != NULL)
		contigfree(m->scratch, 2 * MES_SCRATCH_SIZE, M_DEVBUF);
	if (m->udb_registered)
		vm_phys_fictitious_unreg_range(rman_get_start(m->dbbar) +
		    AMD_UQ_DOORBELL_USER(0) * 4, rman_get_start(m->dbbar) +
		    AMD_UQ_DOORBELL_USER(AMD_UQ_SLOTS) * 4);
	bus_release_resource(m->sc->dev, SYS_RES_MEMORY, m->dbrid, m->dbbar);
	free(m, M_DEVBUF);
}

/*
 * Undo amd_mes12_up: queues removed, MES and CP halted, the GC idle
 * sequence again (box run 84bd8b3f: after MES the RLC needs it to idle),
 * then the block freed.
 */
void
amd_mes12_down(struct mes *m)
{
	struct atrium_amd_softc *sc;

	if (m == NULL)
		return;
	sc = m->sc;
	(void)grbm_watch(m, "MES running, no work", 500);
	amd_gfx12_pm_report(sc, "MES running");
	mes_teardown(m);
	mes_report(m, "torn down");
	amd_gfx12_pm_report(sc, "MES torn down");
	(void)grbm_watch(m, "MES halted", 500);
	device_printf(sc->dev, "mes: GC idle sequence again after MES: %d\n",
	    amd_gc_idle_enter(sc));
	DELAY(100000);
	amd_gfx12_pm_report(sc, "100 ms after the idle sequence");
	if ((m->pci_cmd & PCIM_CMD_BUSMASTEREN) == 0)
		pci_disable_busmaster(sc->dev);
	mes_free(m);
}

/*
 * Stage 4d test: MES up and answering, then halted; 4e's dispatch in
 * between when kenv hw.atrium_gpu.dispatch=1. Run inside the IH test (4b)
 * to catch MES's completion interrupts. Undone by the halt here and the
 * mode 1 reset after.
 */
int
amd_mes12_test(struct atrium_amd_softc *sc, struct amd_gart *gt)
{
	struct mes *m;
	int err, disp = 0;

	TUNABLE_INT_FETCH("hw.atrium_gpu.dispatch", &disp);
	err = amd_mes12_up(sc, gt, disp != 0, &m);
	/* Stage 4e (kenv hw.atrium_gpu.dispatch=1): the first dispatch. */
	if (disp && err == 0)
		(void)amd_mes12_selftest(m);
	else if (disp)
		device_printf(sc->dev, "dispatch: stage 4e needs MES up; "
		    "skipped\n");
	if (m != NULL)
		amd_mes12_down(m);
	device_printf(sc->dev, "mes: stage 4d: %s\n", err == 0 ?
	    "MES up, both pipes answered" : "FAILED");
	return (err);
}
