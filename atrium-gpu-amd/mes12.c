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
	/* NBIF segment 2 */
	uint32_t nbif_db_aper_en, nbif_db_entry0, nbif_db_entry3;
	/* Doorbell dword indices (amdgpu_doorbell.h NAVI10 MES_RING0/1 << 1). */
	uint32_t db_pipe0, db_pipe1, db_aggregated;
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
	  0x00c0, 0x01cb, 0x01ce,
	  0x0b << 1, 0x0c << 1, 0x400 },
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
#define BLK_RES1(p)		(0x5000u + (p) * 0x1000u)
#define BLK_SIZE		0x7000u
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
#define AQ_DOORBELL		72
#define AQ_MQD_ADDR		80
#define AQ_WPTR_ADDR		88
#define AQ_QUEUE_TYPE		112
#define AQ_FLAGS		148	/* dw 37 */
#define AQ_MAP_LEGACY_KQ	0x00002000u
#define AQ_API_STATUS		152
#define AQ_PIPE_ID		200
#define AQ_QUEUE_ID		204
#define QUEUE_TYPE_SCHQ		3
#define RQ_DOORBELL		4
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
	uint8_t *blk;		/* KVA of the GART block */
	vm_paddr_t blk_pa;
	uint64_t va;		/* GART address of the block */
	uint64_t uc_start;
	uint32_t version[2];
	uint32_t cpc_int_cntl[2];	/* as found, restored on teardown */
	bool sched_mapped;		/* pipe 0's queue mapped by MES */
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
	for (p = 0; p < 2; p++) {
		grbm_select(m, 3, p);
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
		/* MEC1 pipes 0-1, 4 queues each, none kept for the kernel. */
		put32(b, HW_COMPUTE_HQD + 0, 0xf);
		put32(b, HW_COMPUTE_HQD + 4, 0xf);
		/*
		 * No GFX queues for MES (gfx_hqd_mask 0). Box run 6c90468a: given
		 * 0xfe as amdgpu does, MES posted a GFX wptr the moment it took
		 * its resources; with PFP/ME halted (4c's resting state) the CPF
		 * stayed busy (CP_CPF_STATUS 0x84000001: CPF_BUSY, CPF_GFX_BUSY,
		 * POST_WPTR_GFX_BUSY), GRBM never idled and the PMFW held gfx at
		 * 3.4 GHz / 60 W. GFX queues come with the GFX ring stage.
		 */
		put32(b, HW_GFX_HQD, 0);
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
static void
hqd_dequeue(struct mes *m, int pipe)
{
	const struct mes_gen *g = m->g;
	uint32_t d;
	int us = 0;

	grbm_select(m, 3, pipe);
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
	device_printf(m->sc->dev, "mes: pipe %d HQD dequeued: active %u%s\n",
	    pipe, d & 1, us >= 100000 ? " (dequeue timed out)" : "");
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
}

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
	int err, p, i;

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
	mes_enable(m, true);
	/*
	 * Both pipes must be running the firmware before any queue is made
	 * active on them. Box run f4265f28: pipe 1 was still at instr 0x1,
	 * GP3 0 500 us after the start; its HQD was activated anyway, the
	 * wptr post wedged CPF and CPC, and the dequeue timed out. Wait for
	 * each pipe's firmware version (GP3) to appear, up to 100 ms.
	 */
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
		if (gp3 == 0) {
			mes_report(m, "pipe not started");
			mes_placement(m, "pipe not started");
			amd_gmc12_fault_report(m->gt, "pipe not started");
			device_printf(sc->dev, "mes: pipe %d firmware not running "
			    "100 ms after the start; no queues made active\n", p);
			return (ENXIO);
		}
		device_printf(sc->dev, "mes: pipe %d firmware up (version %#x) "
		    "%d us after the start wait began\n", p, gp3, us);
	}
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
 * Stage 4d test: MES up and answering, then halted. GC powered, hubs and
 * GART up (4a), GFX set up with the CP halted (4c). Run inside the IH test
 * (4b) to catch MES's completion interrupts. Undone by the halt here and
 * the mode 1 reset after.
 */
int
amd_mes12_test(struct atrium_amd_softc *sc, struct amd_gart *gt)
{
	struct mes m;
	uint32_t i;
	uint16_t pci_cmd;
	int err;

	memset(&m, 0, sizeof(m));
	m.sc = sc;
	m.gt = gt;
	m.g = mes_gen_find(sc);
	if (m.g == NULL) {
		device_printf(sc->dev, "mes: no table for GC %u.%u.%u; not "
		    "touching it\n", sc->ip.gc.major, sc->ip.gc.minor,
		    sc->ip.gc.rev);
		return (ENOTSUP);
	}
	if (!gt->enabled)
		return (ENXIO);
	if ((err = mes_fw_start(&m)) != 0)
		return (err);
	m.r[0].pipe = 0;
	m.r[0].db = m.g->db_pipe0;
	m.r[1].pipe = 1;
	m.r[1].db = m.g->db_pipe1;

	m.dbrid = PCIR_BAR(2);
	m.dbbar = bus_alloc_resource_any(sc->dev, SYS_RES_MEMORY, &m.dbrid,
	    RF_ACTIVE);
	if (m.dbbar == NULL || rman_get_size(m.dbbar) <
	    (bus_size_t)(m.g->db_aggregated + 16) * 4) {
		device_printf(sc->dev, "mes: doorbell BAR unusable\n");
		err = ENXIO;
		goto out_bar;
	}
	m.blk = contigmalloc(BLK_SIZE, M_DEVBUF, M_WAITOK | M_ZERO, 0,
	    ~(vm_paddr_t)0, PAGE_SIZE, 0);
	m.blk_pa = vtophys(m.blk);
	m.va = gt->gart_start + MES_GART_VA;
	for (i = 0; i < BLK_SIZE / PAGE_SIZE; i++)
		(void)amd_gmc12_gart_map(gt, m.va + i * PAGE_SIZE,
		    m.blk_pa + i * PAGE_SIZE);
	if ((err = amd_gmc12_flush(gt)) != 0)
		goto out_unmap;
	device_printf(sc->dev, "mes: %s, start %#jx, block at GART %#jx (pa "
	    "%#jx), doorbell BAR %ju KiB\n", m.g->name, (uintmax_t)m.uc_start,
	    (uintmax_t)m.va, (uintmax_t)m.blk_pa,
	    (uintmax_t)(rman_get_size(m.dbbar) >> 10));

	/* MES reads its rings and writes fences in system memory. */
	pci_cmd = pci_read_config(sc->dev, PCIR_COMMAND, 2);
	pci_enable_busmaster(sc->dev);
	err = mes_run(&m);
	mes_report(&m, err == 0 ? "answered" : "failed");
	(void)grbm_watch(&m, "MES running, no work", 500);
	amd_gfx12_pm_report(sc, "MES running");
	mes_teardown(&m);
	mes_report(&m, "torn down");
	amd_gfx12_pm_report(sc, "MES torn down");
	(void)grbm_watch(&m, "MES halted", 500);
	/*
	 * Box run 84bd8b3f: once MES had run, RLC_STAT fell from 0x35 to 0 and
	 * GRBM_STATUS kept GUI_ACTIVE|CP_BUSY|ANY_ACTIVE after the halt; the
	 * card stayed at 60 W. Redo the GC idle sequence (SRM + clock gating)
	 * so the idle check after stage 4 sees whether that brings it back.
	 */
	device_printf(sc->dev, "mes: GC idle sequence again after MES: %d\n",
	    amd_gc_idle_enter(sc));
	DELAY(100000);
	amd_gfx12_pm_report(sc, "100 ms after the idle sequence");
	if ((pci_cmd & PCIM_CMD_BUSMASTEREN) == 0)
		pci_disable_busmaster(sc->dev);
	device_printf(sc->dev, "mes: stage 4d: %s\n", err == 0 ?
	    "MES up, both pipes answered" : "FAILED");
out_unmap:
	for (i = 0; i < BLK_SIZE / PAGE_SIZE; i++) {
		uint64_t idx = (m.va - gt->gart_start) / PAGE_SIZE + i;

		bus_write_4(gt->vram, gt->table_off + idx * 8, 0);
		bus_write_4(gt->vram, gt->table_off + idx * 8 + 4, 0);
	}
	(void)amd_gmc12_flush(gt);
	contigfree(m.blk, BLK_SIZE, M_DEVBUF);
out_bar:
	if (m.dbbar != NULL)
		bus_release_resource(sc->dev, SYS_RES_MEMORY, m.dbrid,
		    m.dbbar);
	return (err);
}
