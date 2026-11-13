/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * gmc12.c — stage 4a of the silicon bring-up (docs/spec/r9700-bare-metal.md
 * §4.4): the two GPUVM hubs (GFXHUB in GC 12, MMHUB 4.1) and the GART, the
 * VMID 0 page table every engine uses for the driver's own buffers.
 *
 * Interface: Linux's MIT-licensed gmc_v12_0.c, gfxhub_v12_0.c and
 * mmhub_v4_1_0.c (offsets gc_12_0_0_offset.h, mmhub_4_1_0_offset.h; fields
 * checked identical in both *_sh_mask.h for every field written here). The
 * two hubs differ only in register offsets, the L2_CNTL3 default, the retry
 * fault interrupt and the invalidate semaphore (MMHUB only), so one routine
 * programs either from a row of hub_gens[], picked by discovered version.
 *
 * Layout (amdgpu's, for a dGPU without XGMI): VRAM at the FB location the
 * VBIOS left in MMHUB (fb_start..fb_end), AGP off, a 512 MiB GART at MC
 * address 0, one level (depth 0), its table in VRAM inside the driver's own
 * area. Unmapped GART pages read as invalid and fault to the default page.
 *
 * Contexts 1-15 (stage 5b) are set up as gfxhub_v12_0_setup_vmid_config
 * does: 48-bit VA, four levels (PDB2, PDB1, PDB0, PTB, 9 bits each), no
 * retry, faults to the default page. A context gets a page-table base only
 * when a VM is bound to it (amd_gmc12_vm_bind, or MES for VMIDs 8-15). The
 * per-client page tables are built here too (amd_gmc12_vm_*), in a VRAM
 * pool written through BAR0, in the gfx12 format of gmc_v12_0.c and
 * amdgpu_vm_pt.c.
 */
#include "atrium_gpu_amd.h"

#include <vm/vm.h>
#include <vm/pmap.h>

/*
 * One TLB invalidation at a time on the hubs' engine 17 (amdgpu's
 * gmc.invalidate_lock around gmc_v12_0_flush_vm_hub). GFXHUB has no
 * semaphore; two threads writing INV_REQ while the other waits for its ack
 * left the engine never acking again (box run aea5d54a: a lost queue's
 * eviction flush and its VM's BO-free flush, interleaved, the log naming
 * one context with the next one's request).
 */
static struct mtx gmc12_inv_mtx;
MTX_SYSINIT(gmc12_inv, &gmc12_inv_mtx, "gmc12 inv", MTX_DEF);

/* Layout inside the driver's VRAM area (psp.c stages at 68 MiB, smu.c's table at 84). */
#define GART_TABLE_VRAM_OFF	(88u << 20)
#define GART_SIZE		(512ull << 20)
#define GART_PAGES		(GART_SIZE >> PAGE_SHIFT)
#define GART_TABLE_SIZE		(GART_PAGES * 8)		/* 1 MiB */
#define SCRATCH_VRAM_OFF	(GART_TABLE_VRAM_OFF + GART_TABLE_SIZE)
/* Page-table pool for per-client VMs (stage 5b): 256 KiB at 92 MiB. */
#define VM_POOL_VRAM_OFF	(92u << 20)
#define VM_POOL_PAGES		64u
#define VM_SLICE_PAGES		16u		/* tables per VM (test VMs) */
#define VM_SLICES		(VM_POOL_PAGES / VM_SLICE_PAGES)
/* The VRAM allocator (5d) starts above all of this. */
CTASSERT(VM_POOL_VRAM_OFF + VM_POOL_PAGES * PAGE_SIZE <= AMD_SI_VRAM_DRIVER_END);

/* PTE bits (amdgpu_vm.h), GFX12 layout. */
#define PTE_VALID		(1ull << 0)
#define PTE_SYSTEM		(1ull << 1)
#define PTE_SNOOPED		(1ull << 2)
#define PTE_EXECUTABLE		(1ull << 4)
#define PTE_READABLE		(1ull << 5)
#define PTE_WRITEABLE		(1ull << 6)
#define PTE_MTYPE_UC		(3ull << 54)
#define PTE_MTYPE_MASK		(3ull << 54)	/* NC is 0 */
#define PTE_IS_PTE		(1ull << 63)
#define PDE_BFS(x)		((uint64_t)((x) & 0x1f) << 58)
#define PDE_ADDR_MASK		0x0000ffffffffffc0ull
#define PTE_ADDR_MASK		0x0000fffffffff000ull
/* VM geometry (amdgpu_vm_adjust_size(256 TiB, 9, 3, 48)). */
#define VM_LEVELS		4		/* PDB2, PDB1, PDB0, PTB */
#define VM_DEPTH		3		/* PAGE_TABLE_DEPTH */
#define VM_VA_BITS		48
#define VM_MAX_PFN		(1ull << (VM_VA_BITS - PAGE_SHIFT))
#define NUM_CTX			16

/* Fields (identical in GCVM_* and MMVM_*). */
#define L1_TLB_ENABLE		0x00000001
#define L1_TLB_SYSTEM_ACCESS	0x00000018	/* mode 3 */
#define L1_TLB_UNMAPPED_ACCESS	0x00000020
#define L1_TLB_ADVANCED_MODEL	0x00000040
#define L1_TLB_ECO_BITS		0x00000780
#define L1_TLB_MTYPE		0x00001800
#define L2_ENABLE		0x00000001
#define L2_FRAGMENT		0x00000002
#define L2_PDE0_TAG_MODE	0x00000100
#define L2_DEFAULT_PAGE_OUT	0x00000800
#define L2_PDE_FAULT_CLASS	0x00040000
#define L2_CTX1_IDENTITY_MODE	0x00180000
#define L2_IDENTITY_FRAG	0x03e00000
#define L2_CNTL2_INV_L1_TLBS	0x00000001
#define L2_CNTL2_INV_L2_CACHE	0x00000002
#define L2_CNTL3_BANK_SELECT	0x0000003f
#define L2_CNTL3_BIGK_FRAG	0x000f8000
#define L2_CNTL4_PDE_PHYS	0x00000040
#define L2_CNTL4_PTE_PHYS	0x00000080
#define L2_CNTL5_SMALLK_FRAG	0x0000001f
#define FAULT_CNTL2_PTE_READ_RETRY 0x00040000
#define FAULT_CNTL2_RETRY_INT	0x00080000
#define FAULT_CNTL_DEFAULTS	0x00001ffc	/* RANGE..EXECUTE _ENABLE_DEFAULT */
#define CTX_ENABLE		0x00000001
#define CTX_DEPTH		0x00000006
#define CTX_RETRY		0x00000100
#define CTX_BLOCK_SIZE		0x000000f0
#define CTX_FAULT_DEFAULTS	0x00aaa800	/* *_FAULT_ENABLE_DEFAULT */
#define CTX_FAULT_INTERRUPTS	0x00555400	/* *_FAULT_ENABLE_INTERRUPT */
#define FAULT_CNTL_CLEAR	0x00000001
#define FAULT_STATUS_VMID(s)	(((s) >> 20) & 0xf)
#define FAULT_STATUS_CID(s)	(((s) >> 9) & 0x1ff)
#define INV_L2_PTES		0x00080000
#define INV_L2_PDE0		0x00100000
#define INV_L2_PDE1		0x00200000
#define INV_L2_PDE2		0x00400000
#define INV_L1_PTES		0x00800000
#define INV_ENG_GART		17

struct hub_gen {
	const char *name;
	/* Which discovered IP and version the row is for. */
	bool	gc;			/* true: GC's base, false: MMHUB's */
	uint8_t	major, minor;
	uint32_t fb_base, fb_top, fb_offset, agp_base, agp_bot, agp_top;
	uint32_t sys_low, sys_high, sys_def_lsb, sys_def_msb, l1_tlb;
	uint32_t fault_def_lo, fault_def_hi, fault_cntl, fault_cntl2;
	uint32_t fault_status, fault_addr_lo, fault_addr_hi;
	uint32_t l2_cntl, l2_cntl2, l2_cntl3, l2_cntl4, l2_cntl5;
	uint32_t ctx0_cntl, ctx0_pt_base_lo, ctx0_pt_base_hi;
	uint32_t ctx0_pt_start_lo, ctx0_pt_start_hi, ctx0_pt_end_lo,
	    ctx0_pt_end_hi;
	uint32_t id_low_lo, id_low_hi, id_high_lo, id_high_hi, id_off_lo,
	    id_off_hi;
	uint32_t inv_sem, inv_req, inv_ack, inv_range_lo, inv_range_hi;
	uint32_t eng_distance, eng_addr_distance;
	uint32_t l2_cntl3_default;
	bool	retry_int, use_sem;
	/*
	 * amdgpu's gmc.translate_further for this GC: set only for GC 12.1
	 * (5-level); GC 12.0.x leaves it off. It picks the L2 bank select /
	 * BigK fragment (12/9 vs 9/6) and whether PDB1 entries carry BFS 9.
	 */
	bool	translate_further;
};

static const struct hub_gen hub_gens[] = {
	{
		.name = "GFXHUB (GC 12.0)", .gc = true, .major = 12, .minor = 0,
		.fb_base = 0x1614, .fb_top = 0x1615, .fb_offset = 0x15a7,
		.agp_base = 0x1618, .agp_bot = 0x1617, .agp_top = 0x1616,
		.sys_low = 0x1619, .sys_high = 0x161a, .sys_def_lsb = 0x15a8,
		.sys_def_msb = 0x15a9, .l1_tlb = 0x161b,
		.fault_def_lo = 0x15d4, .fault_def_hi = 0x15d5,
		.fault_cntl = 0x15cc, .fault_cntl2 = 0x15cd,
		.fault_status = 0x15d0, .fault_addr_lo = 0x15d2,
		.fault_addr_hi = 0x15d3,
		.l2_cntl = 0x15c4, .l2_cntl2 = 0x15c5, .l2_cntl3 = 0x15c6,
		.l2_cntl4 = 0x15dd, .l2_cntl5 = 0x15e3,
		.ctx0_cntl = 0x1624, .ctx0_pt_base_lo = 0x168f,
		.ctx0_pt_base_hi = 0x1690, .ctx0_pt_start_lo = 0x16af,
		.ctx0_pt_start_hi = 0x16b0, .ctx0_pt_end_lo = 0x16cf,
		.ctx0_pt_end_hi = 0x16d0,
		.id_low_lo = 0x15d7, .id_low_hi = 0x15d8, .id_high_lo = 0x15d9,
		.id_high_hi = 0x15da, .id_off_lo = 0x15db, .id_off_hi = 0x15dc,
		.inv_sem = 0x1635, .inv_req = 0x1647, .inv_ack = 0x1659,
		.inv_range_lo = 0x166b, .inv_range_hi = 0x166c,
		.eng_distance = 1, .eng_addr_distance = 2,
		.l2_cntl3_default = 0x80120007, .retry_int = true,
		.use_sem = false, .translate_further = false,
	},
	{
		.name = "MMHUB 4.1", .gc = false, .major = 4, .minor = 1,
		.fb_base = 0x0554, .fb_top = 0x0555, .fb_offset = 0x04c7,
		.agp_base = 0x0558, .agp_bot = 0x0557, .agp_top = 0x0556,
		.sys_low = 0x0559, .sys_high = 0x055a, .sys_def_lsb = 0x04c8,
		.sys_def_msb = 0x04c9, .l1_tlb = 0x055b,
		.fault_def_lo = 0x04f4, .fault_def_hi = 0x04f5,
		.fault_cntl = 0x04ec, .fault_cntl2 = 0x04ed,
		.fault_status = 0x04f0, .fault_addr_lo = 0x04f2,
		.fault_addr_hi = 0x04f3,
		.l2_cntl = 0x04e4, .l2_cntl2 = 0x04e5, .l2_cntl3 = 0x04e6,
		.l2_cntl4 = 0x04fd, .l2_cntl5 = 0x0503,
		.ctx0_cntl = 0x0564, .ctx0_pt_base_lo = 0x05cf,
		.ctx0_pt_base_hi = 0x05d0, .ctx0_pt_start_lo = 0x05ef,
		.ctx0_pt_start_hi = 0x05f0, .ctx0_pt_end_lo = 0x060f,
		.ctx0_pt_end_hi = 0x0610,
		.id_low_lo = 0x04f7, .id_low_hi = 0x04f8, .id_high_lo = 0x04f9,
		.id_high_hi = 0x04fa, .id_off_lo = 0x04fb, .id_off_hi = 0x04fc,
		.inv_sem = 0x0575, .inv_req = 0x0587, .inv_ack = 0x0599,
		.inv_range_lo = 0x05ab, .inv_range_hi = 0x05ac,
		.eng_distance = 1, .eng_addr_distance = 2,
		.l2_cntl3_default = 0x80100007, .retry_int = false,
		.use_sem = true, .translate_further = false,
	},
};

struct hub {
	struct atrium_amd_softc *sc;
	const struct hub_gen *g;
	bus_size_t base;	/* byte offset of the IP's segment 0 */
};

static const struct hub_gen *
hub_find(struct atrium_amd_softc *sc, bool gc)
{
	const struct amd_ip_ver *ip = gc ? &sc->ip.gc : &sc->ip.mmhub;
	size_t i;

	if (!ip->present || ip->nbase < 1)
		return (NULL);
	for (i = 0; i < nitems(hub_gens); i++)
		if (hub_gens[i].gc == gc && hub_gens[i].major == ip->major &&
		    hub_gens[i].minor == ip->minor)
			return (&hub_gens[i]);
	return (NULL);
}

static bool
hub_open(struct atrium_amd_softc *sc, bool gc, struct hub *h)
{
	const struct amd_ip_ver *ip = gc ? &sc->ip.gc : &sc->ip.mmhub;

	h->sc = sc;
	h->g = hub_find(sc, gc);
	if (h->g == NULL)
		return (false);
	h->base = (bus_size_t)ip->base[0] * 4;
	/* The highest register is context 15's end_hi or engine 17's inv_*. */
	return (h->base + (bus_size_t)MAX(h->g->ctx0_pt_end_hi + 2 *
	    (NUM_CTX - 1), h->g->inv_range_lo + h->g->eng_addr_distance * 17 +
	    1) * 4 + 4 <=
	    rman_get_size(sc->regs));
}

static uint32_t
hr(struct hub *h, uint32_t dw)
{
	return (amd_mmio_read32(h->sc, h->base + (bus_size_t)dw * 4));
}

static void
hw(struct hub *h, uint32_t dw, uint32_t v)
{
	amd_mmio_write32(h->sc, h->base + (bus_size_t)dw * 4, v);
}

/*
 * The BAR byte offset of VM context `ctx`'s PAGE_TABLE_BASE_LO on the GC hub
 * (gc) or the MMHUB, from this IP version's table; 0 if there is none. For
 * the hostile-PM4 test's target list (smu.c, si_protected_regs).
 */
bus_size_t
amd_gmc12_ctx_base_reg(struct atrium_amd_softc *sc, bool gc, u_int ctx)
{
	struct hub h;

	if (ctx >= NUM_CTX || !hub_open(sc, gc, &h))
		return (0);
	return (h.base + (bus_size_t)(h.g->ctx0_pt_base_lo + 2 * ctx) * 4);
}

static void
hrmw(struct hub *h, uint32_t dw, uint32_t clr, uint32_t set)
{
	hw(h, dw, (hr(h, dw) & ~clr) | set);
}

/* gfxhub_v12_0_gart_enable / mmhub_v4_1_0_gart_enable, context 0 only. */
static void
hub_gart_enable(struct hub *h, const struct amd_gart *gt)
{
	const struct hub_gen *g = h->g;
	uint64_t v;
	u_int i;

	/* GART aperture: context 0's page table, depth 0. */
	hw(h, g->ctx0_pt_base_lo, (uint32_t)gt->pd_addr);
	hw(h, g->ctx0_pt_base_hi, (uint32_t)(gt->pd_addr >> 32));
	hw(h, g->ctx0_pt_start_lo, (uint32_t)(gt->gart_start >> 12));
	hw(h, g->ctx0_pt_start_hi, (uint32_t)(gt->gart_start >> 44));
	hw(h, g->ctx0_pt_end_lo, (uint32_t)(gt->gart_end >> 12));
	hw(h, g->ctx0_pt_end_hi, (uint32_t)(gt->gart_end >> 44));

	/* System aperture: AGP off (bot > top), FB only. */
	hw(h, g->agp_base, 0);
	hw(h, g->agp_bot, (uint32_t)(0xffffffffffffull >> 24));
	hw(h, g->agp_top, 0);
	hw(h, g->sys_low, (uint32_t)(gt->fb_start >> 18));
	hw(h, g->sys_high, (uint32_t)(gt->fb_end >> 18));
	v = gt->scratch_mc - gt->fb_start + gt->vram_base_offset;
	hw(h, g->sys_def_lsb, (uint32_t)(v >> 12));
	hw(h, g->sys_def_msb, (uint32_t)(v >> 44));
	hw(h, g->fault_def_lo, (uint32_t)(gt->dummy_phys >> 12));
	hw(h, g->fault_def_hi, (uint32_t)(gt->dummy_phys >> 44));
	hrmw(h, g->fault_cntl2, 0, FAULT_CNTL2_PTE_READ_RETRY |
	    (g->retry_int ? FAULT_CNTL2_RETRY_INT : 0));

	/* L1 TLB: on, system access mode 3, advanced driver model, UC. */
	hrmw(h, g->l1_tlb, L1_TLB_SYSTEM_ACCESS | L1_TLB_UNMAPPED_ACCESS |
	    L1_TLB_ECO_BITS | L1_TLB_MTYPE, L1_TLB_ENABLE |
	    L1_TLB_SYSTEM_ACCESS | L1_TLB_ADVANCED_MODEL | (3u << 11));

	/* L2 cache (init_cache_regs; bank select / BigK per translate_further). */
	hrmw(h, g->l2_cntl, L2_FRAGMENT | L2_PDE0_TAG_MODE |
	    L2_PDE_FAULT_CLASS | L2_CTX1_IDENTITY_MODE | L2_IDENTITY_FRAG,
	    L2_ENABLE | L2_DEFAULT_PAGE_OUT | (1u << 19));
	hrmw(h, g->l2_cntl2, 0, L2_CNTL2_INV_L1_TLBS | L2_CNTL2_INV_L2_CACHE);
	hw(h, g->l2_cntl3, (g->l2_cntl3_default &
	    ~(L2_CNTL3_BANK_SELECT | L2_CNTL3_BIGK_FRAG)) |
	    (g->translate_further ? 12u | (9u << 15) : 9u | (6u << 15)));
	hw(h, g->l2_cntl4, 0x000000c1 & ~(L2_CNTL4_PDE_PHYS |
	    L2_CNTL4_PTE_PHYS));
	hw(h, g->l2_cntl5, 0x00003fe0 & ~L2_CNTL5_SMALLK_FRAG);

	/* System domain: context 0 on, depth 0, no retry. */
	hrmw(h, g->ctx0_cntl, CTX_DEPTH | CTX_RETRY, CTX_ENABLE);

	/*
	 * Contexts 1-15 (setup_vmid_config): on, depth 3, block size 9 - 9,
	 * every fault to the default page, no retry (noretry), the whole
	 * 48-bit range. No base yet: a context is used only once bound.
	 */
	for (i = 1; i < NUM_CTX; i++) {
		hrmw(h, g->ctx0_cntl + i, CTX_DEPTH | CTX_BLOCK_SIZE |
		    CTX_RETRY | CTX_FAULT_INTERRUPTS, CTX_ENABLE |
		    (VM_DEPTH << 1) | CTX_FAULT_DEFAULTS);
		hw(h, g->ctx0_pt_start_lo + 2 * i, 0);
		hw(h, g->ctx0_pt_start_hi + 2 * i, 0);
		hw(h, g->ctx0_pt_end_lo + 2 * i, (uint32_t)(VM_MAX_PFN - 1));
		hw(h, g->ctx0_pt_end_hi + 2 * i,
		    (uint32_t)((VM_MAX_PFN - 1) >> 32));
	}

	/* Identity aperture off. */
	hw(h, g->id_low_lo, 0xffffffff);
	hw(h, g->id_low_hi, 0x0000000f);
	hw(h, g->id_high_lo, 0);
	hw(h, g->id_high_hi, 0);
	hw(h, g->id_off_lo, 0);
	hw(h, g->id_off_hi, 0);

	/* Every invalidation engine covers the whole range. */
	for (i = 0; i < 18; i++) {
		hw(h, g->inv_range_lo + i * g->eng_addr_distance, 0xffffffff);
		hw(h, g->inv_range_hi + i * g->eng_addr_distance, 0x1f);
	}

	/* Faults go to the default page (amdgpu_vm_fault_stop default). */
	hrmw(h, g->fault_cntl, 0, FAULT_CNTL_DEFAULTS);
}

/*
 * gmc_v12_0_flush_vm_hub on engine 17 (GART): request, wait for the ack
 * bit for `vmid`; MMHUB takes the semaphore around it. 0 or ETIMEDOUT.
 */
static int
hub_flush(struct hub *h, u_int vmid)
{
	const struct hub_gen *g = h->g;
	uint32_t e = INV_ENG_GART * g->eng_distance;
	int us, err = 0;

	if (g->use_sem) {
		for (us = 0; us < 100000; us++) {
			if (hr(h, g->inv_sem + e) & 1)
				break;
			DELAY(1);
		}
		if (us == 100000)
			err = ETIMEDOUT;
	}
	hw(h, g->inv_req + e, (1u << vmid) | INV_L2_PTES | INV_L2_PDE0 |
	    INV_L2_PDE1 | INV_L2_PDE2 | INV_L1_PTES);
	for (us = 0; us < 100000; us++) {
		if (hr(h, g->inv_ack + e) & (1u << vmid))
			break;
		DELAY(1);
	}
	if (us == 100000)
		err = ETIMEDOUT;
	if (g->use_sem)
		hw(h, g->inv_sem + e, 0);
	return (err);
}

static void
hub_report(struct hub *h, const char *when)
{
	const struct hub_gen *g = h->g;

	device_printf(h->sc->dev, "gmc: %s %s: CONTEXT0_CNTL %#x, PT base "
	    "%#x%08x, L1_TLB %#x, L2_CNTL %#x, fault status %#x (addr "
	    "%#x%08x)\n", g->name, when, hr(h, g->ctx0_cntl),
	    hr(h, g->ctx0_pt_base_hi), hr(h, g->ctx0_pt_base_lo),
	    hr(h, g->l1_tlb), hr(h, g->l2_cntl), hr(h, g->fault_status),
	    hr(h, g->fault_addr_hi), hr(h, g->fault_addr_lo));
}

/* A context flush that never got its ack: what the hub looks like (box
 * run 1b71cafd: after a hostile shader every flush timed out). */
static void
hub_flush_report(struct hub *h, u_int vmid)
{
	const struct hub_gen *g = h->g;
	uint32_t e = INV_ENG_GART * g->eng_distance;

	device_printf(h->sc->dev, "gmc: %s flush of context %u timed out: "
	    "INV_REQ %#x, INV_ACK %#x%s\n", g->name, vmid,
	    hr(h, g->inv_req + e), hr(h, g->inv_ack + e), g->use_sem ?
	    (hr(h, g->inv_sem + e) & 1 ? ", semaphore held" :
	    ", semaphore free") : "");
	hub_report(h, "after the timeout");
}

/*
 * Map one system page into the GART (amdgpu_gart_bind for a system page:
 * valid, system, snooped, readable, writeable, plus the GART's UC,
 * executable, is-PTE). Caller flushes HDP and the TLBs afterwards.
 */
int
amd_gmc12_gart_map(struct amd_gart *gt, uint64_t gart_addr, vm_paddr_t pa)
{
	uint64_t idx, pte;

	if (gart_addr < gt->gart_start || gart_addr > gt->gart_end ||
	    (gart_addr & PAGE_MASK) != 0 || (pa & PAGE_MASK) != 0)
		return (EINVAL);
	idx = (gart_addr - gt->gart_start) >> PAGE_SHIFT;
	pte = (uint64_t)pa | PTE_VALID | PTE_SYSTEM | PTE_SNOOPED |
	    PTE_READABLE | PTE_WRITEABLE | PTE_EXECUTABLE | PTE_MTYPE_UC |
	    PTE_IS_PTE;
	bus_write_4(gt->vram, gt->table_off + idx * 8, (uint32_t)pte);
	bus_write_4(gt->vram, gt->table_off + idx * 8 + 4,
	    (uint32_t)(pte >> 32));
	return (0);
}

/* Both hubs' VMID 0 fault status and address, for callers' diagnostics. */
void
amd_gmc12_fault_report(struct amd_gart *gt, const char *when)
{
	struct hub mm, gfx;

	if (hub_open(gt->sc, false, &mm))
		hub_report(&mm, when);
	if (hub_open(gt->sc, true, &gfx))
		hub_report(&gfx, when);
}

int
amd_gmc12_flush(struct amd_gart *gt)
{
	struct hub mm, gfx;
	int err = 0;

	mtx_lock(&gmc12_inv_mtx);
	amd_hdp_flush(gt->sc);
	if (hub_open(gt->sc, false, &mm) && hub_flush(&mm, 0) != 0)
		err = ETIMEDOUT;
	if (hub_open(gt->sc, true, &gfx) && hub_flush(&gfx, 0) != 0)
		err = ETIMEDOUT;
	mtx_unlock(&gmc12_inv_mtx);
	return (err);
}

/*
 * Stage 4a: GART layout, table, both hubs, TLB flush with ack. GC must be
 * powered (the caller holds GfxOff off). `vram` is BAR0; the table and the
 * scratch page are written through it.
 */
int
amd_gmc12_gart_enable(struct atrium_amd_softc *sc, struct resource *vram,
    struct amd_gart *gt)
{
	struct hub mm, gfx;
	uint32_t memsize_mb;
	int err;

	memset(gt, 0, sizeof(*gt));
	gt->sc = sc;
	gt->vram = vram;
	if (!hub_open(sc, false, &mm) || !hub_open(sc, true, &gfx)) {
		device_printf(sc->dev, "gmc: no hub table for GC %u.%u / MMHUB "
		    "%u.%u; not touching them\n", sc->ip.gc.major,
		    sc->ip.gc.minor, sc->ip.mmhub.major, sc->ip.mmhub.minor);
		return (ENOTSUP);
	}
	if (rman_get_size(vram) < SCRATCH_VRAM_OFF + PAGE_SIZE)
		return (ENXIO);

	/* Where the VBIOS put VRAM in the MC address space (MMHUB's view). */
	gt->fb_start = (uint64_t)(hr(&mm, mm.g->fb_base) & 0x00ffffff) << 24;
	gt->fb_end = ((uint64_t)(hr(&mm, mm.g->fb_top) & 0x00ffffff) << 24) |
	    0xffffff;
	gt->vram_base_offset = (uint64_t)hr(&mm, mm.g->fb_offset) << 24;
	memsize_mb = (uint32_t)((gt->fb_end - gt->fb_start + 1) >> 20);
	if (gt->fb_start < GART_SIZE || gt->fb_end <= gt->fb_start) {
		device_printf(sc->dev, "gmc: FB location %#jx-%#jx leaves no "
		    "room for the GART below it\n", (uintmax_t)gt->fb_start,
		    (uintmax_t)gt->fb_end);
		return (ENXIO);
	}
	/* amdgpu_gmc_gart_location(LOW): below VRAM, at 0. */
	gt->gart_start = 0;
	gt->gart_end = GART_SIZE - 1;
	gt->table_off = GART_TABLE_VRAM_OFF;
	gt->table_mc = gt->fb_start + GART_TABLE_VRAM_OFF;
	gt->scratch_mc = gt->fb_start + SCRATCH_VRAM_OFF;
	/* PDB address: physical FB offset of the table, valid. */
	gt->pd_addr = (gt->table_mc - gt->fb_start + gt->vram_base_offset) |
	    PTE_VALID;

	/* Dummy page for faults: one zeroed system page. */
	gt->dummy = contigmalloc(PAGE_SIZE, M_DEVBUF, M_WAITOK | M_ZERO, 0,
	    ~(vm_paddr_t)0, PAGE_SIZE, 0);
	gt->dummy_phys = vtophys(gt->dummy);

	/* Empty table, scratch page zeroed. */
	bus_set_region_4(vram, gt->table_off, 0, GART_TABLE_SIZE / 4);
	bus_set_region_4(vram, SCRATCH_VRAM_OFF, 0, PAGE_SIZE / 4);
	amd_hdp_flush(sc);

	device_printf(sc->dev, "gmc: VRAM %u MiB at MC %#jx-%#jx (FB offset "
	    "%#jx); GART %ju MiB at %#jx, table at MC %#jx\n", memsize_mb,
	    (uintmax_t)gt->fb_start, (uintmax_t)gt->fb_end,
	    (uintmax_t)gt->vram_base_offset, (uintmax_t)(GART_SIZE >> 20),
	    (uintmax_t)gt->gart_start, (uintmax_t)gt->table_mc);

	hub_report(&mm, "before");
	hub_report(&gfx, "before");
	hub_gart_enable(&mm, gt);
	hub_gart_enable(&gfx, gt);
	err = amd_gmc12_flush(gt);
	hub_report(&mm, "after");
	hub_report(&gfx, "after");
	device_printf(sc->dev, "gmc: TLB flush on both hubs: %s\n",
	    err == 0 ? "acked" : "NO ACK");
	gt->enabled = (err == 0);
	return (err);
}

/* Undo: contexts off, L1 TLB and L2 off (gart_disable); free the dummy. */
void
amd_gmc12_gart_disable(struct amd_gart *gt)
{
	struct hub h;
	int i;

	for (i = 0; i < 2 && gt->sc != NULL; i++) {
		if (!hub_open(gt->sc, i == 1, &h))
			continue;
		for (int c = 0; c < NUM_CTX; c++)
			hrmw(&h, h.g->ctx0_cntl + c, CTX_ENABLE, 0);
		hrmw(&h, h.g->l1_tlb, L1_TLB_ENABLE | L1_TLB_ADVANCED_MODEL, 0);
		hrmw(&h, h.g->l2_cntl, L2_ENABLE, 0);
		hw(&h, h.g->l2_cntl3, 0);
	}
	if (gt->dummy != NULL)
		contigfree(gt->dummy, PAGE_SIZE, M_DEVBUF);
	gt->dummy = NULL;
	gt->enabled = false;
}

/*
 * Stage 5b: per-client page tables, in the gfx12 format of gmc_v12_0.c and
 * amdgpu_vm_pt.c.
 *
 * The amdgpu layout (AMD_VM12_4L_VRAM) is a four-level table (PDB2 root,
 * PDB1, PDB0, PTB; 512 entries each) in a VRAM pool, written through BAR0
 * like the GART table. PDEs point at VRAM by physical FB address and are
 * just VALID (amdgpu_gmc_get_pde_for_bo for a VRAM BO); only with
 * translate_further (GC 12.1, not 12.0) do PDB1 entries also carry block
 * fragment size 9. A new PTB is filled with the invalid-PTE pattern
 * (EXECUTABLE | IS_PTE, amdgpu_vm_pte_update_flags), and so is an
 * unmapped page. A mapped system page is VALID | SYSTEM | SNOOPED plus
 * R/W/X, MTYPE NC (gmc_v12_0_get_vm_pte, AMDGPU_VM_MTYPE_DEFAULT) and
 * IS_PTE (bit 63).
 *
 * IS_PTE on a valid leaf is from silicon, not from my reading of amdgpu:
 * runs 20261112T215929Z and T220611Z faulted the first fetch through VMID
 * 1 (WALKER_ERROR 5, MAPPING_ERROR) with leaves lacking bit 63; run
 * T221309Z built four layouts side by side and only the leaves carrying
 * bit 63 translated (the flat depth-0 table and the four-level VRAM table
 * with IS_PTE both ran write_42; the same tables without it faulted, in
 * VRAM and in system memory). The two diagnostic layouts stay for later
 * bring-ups:
 *  AMD_VM12_FLAT     depth 0, one PTB over a 2 MiB window, GART-style PTEs;
 *  AMD_VM12_4L_SYS   the four-level table with every table in system
 *                    memory (PDEs VALID | SYSTEM | SNOOPED).
 *
 * Test VMs take tables from a bump allocator over a fixed pool (16 per
 * VM, 4 VMs). Client VMs (AMD_VM12_4L_HEAP) take each table as a
 * CPU-visible page from the VRAM allocator, up to AMD_VM12_HEAP_TABLES:
 * the whole BO window (ATRIUM_AMD_VM_MAX_BO pages, 128 leaf tables) plus
 * the directories and the queue pages. Box run 955528b9 hit the pool's
 * 13 leaf tables (6656 pages, va_reuse) with client VMs on it. Tables are
 * never freed before the VM goes; next-fit reuses them as the window wraps.
 */
static bool
vm_sys(const struct amd_vm12 *vm)
{
	return (vm->mode == AMD_VM12_4L_SYS);
}

static bool
vm_heap(const struct amd_vm12 *vm)
{
	return (vm->mode == AMD_VM12_4L_HEAP);
}

/* Table `t`'s FB offset (VRAM-resident modes): also its BAR0 offset. */
static bus_size_t
tbl_off(const struct amd_vm12 *vm, u_int t)
{
	if (vm_heap(vm))
		return ((bus_size_t)vm->heap[t]);
	return (VM_POOL_VRAM_OFF + (bus_size_t)(vm->first + t) * PAGE_SIZE);
}

static uint64_t
tbl_rd(struct amd_vm12 *vm, u_int t, u_int i)
{
	bus_size_t off;

	if (vm_sys(vm))
		return (((volatile uint64_t *)vm->sys[t])[i]);
	off = tbl_off(vm, t) + i * 8;
	return ((uint64_t)bus_read_4(vm->gt->vram, off) |
	    (uint64_t)bus_read_4(vm->gt->vram, off + 4) << 32);
}

static void
tbl_wr(struct amd_vm12 *vm, u_int t, u_int i, uint64_t v)
{
	bus_size_t off;

	if (vm_sys(vm)) {
		((volatile uint64_t *)vm->sys[t])[i] = v;
		return;
	}
	off = tbl_off(vm, t) + i * 8;
	bus_write_4(vm->gt->vram, off, (uint32_t)v);
	bus_write_4(vm->gt->vram, off + 4, (uint32_t)(v >> 32));
}

/* What a PDE (or the context base) pointing at table `t` holds. */
static uint64_t
tbl_pde(struct amd_vm12 *vm, u_int t)
{
	if (vm_sys(vm))
		return ((uint64_t)vm->sys_pa[t] | PTE_VALID | PTE_SYSTEM |
		    PTE_SNOOPED);
	return (((uint64_t)tbl_off(vm, t) + vm->gt->vram_base_offset) |
	    PTE_VALID);
}

/* The table a PDE points at, by looking it up among ours. */
static bool
tbl_of(struct amd_vm12 *vm, uint64_t pde, u_int *t)
{
	u_int i;

	for (i = 0; i < vm->used; i++)
		if ((tbl_pde(vm, i) & PDE_ADDR_MASK) == (pde & PDE_ADDR_MASK)) {
			*t = i;
			return (true);
		}
	return (false);
}

/* Table `t` is one of the VM's empty ("null") tables. */
static bool
tbl_null(const struct amd_vm12 *vm, u_int t)
{
	u_int l;

	for (l = 1; l < VM_LEVELS; l++)
		if (vm->nullt[l] != 0 && vm->nullt[l] == t)
			return (true);
	return (false);
}

/* A new table at level `lvl` (0 the root .. VM_LEVELS - 1 a PTB). */
static int
tbl_alloc(struct amd_vm12 *vm, u_int lvl, u_int *t)
{
	bool ptb = lvl == VM_LEVELS - 1;
	uint64_t fill;
	uint64_t off;
	u_int i;
	int err;

	if (vm->used >= (vm_heap(vm) ? AMD_VM12_HEAP_TABLES : VM_SLICE_PAGES))
		return (ENOMEM);
	*t = vm->used;
	if (vm_heap(vm)) {
		err = amd_vram_alloc(vm->gt->sc, PAGE_SIZE,
		    AMD_VRAM_CPU_VISIBLE, &off);
		if (err != 0)
			return (err);
		vm->heap[*t] = off;
	}
	if (vm_sys(vm)) {
		vm->sys[*t] = contigmalloc(PAGE_SIZE, M_DEVBUF, M_WAITOK |
		    M_ZERO, 0, ~(vm_paddr_t)0, PAGE_SIZE, 0);
		vm->sys_pa[*t] = vtophys(vm->sys[*t]);
	}
	vm->used++;
	/*
	 * Unused entries: a PTB's are invalid PTEs (EXECUTABLE | IS_PTE, the
	 * GMC9 fault-priority workaround); a directory's are invalid leaf
	 * PDEs, 0 | IS_PTE, as amdgpu_vm_pt_clear writes them ("handle leaf
	 * PDEs as PTEs", AMDGPU_PDE_PTE_GFX12). Box runs 1b71cafd/3959e48a:
	 * with 0 there, a store through an empty directory entry (VA 0, a
	 * high VA) faulted with WALKER_ERROR 6 and left the GFXHUB L2 unable
	 * to ack any invalidation until a mode 1 reset.
	 */
	/*
	 * Box run 6eca95c0: with that, a store through an empty PDB2 entry
	 * (0x7ffffff00000) faulted cleanly (WALKER_ERROR 0) and still wedged
	 * the GFXHUB (UTCL2 and EA busy, no invalidation acked). A store past
	 * a buffer, which faults at a 4 KiB PTE, never did. So client VMs
	 * point every empty entry at a chain of empty tables down to an empty
	 * PTB (vm->nullt): every fault, anywhere in the 48-bit range, is a
	 * 4 KiB PTE fault.
	 */
	fill = ptb ? PTE_EXECUTABLE | PTE_IS_PTE : vm->nullt[lvl + 1] != 0 ?
	    tbl_pde(vm, vm->nullt[lvl + 1]) : PTE_IS_PTE;
	for (i = 0; i < PAGE_SIZE / 8; i++)
		tbl_wr(vm, *t, i, fill);
	return (0);
}

/*
 * A client VM: the root (table 0), then its empty tables bottom up (an
 * empty PTB, a PDB0 all pointing at it, a PDB1 all pointing at that), and
 * the root's entries pointed at the empty PDB1. See tbl_alloc.
 */
static int
vm_init_null(struct amd_vm12 *vm)
{
	u_int root, t, l, i;
	int err;

	if ((err = tbl_alloc(vm, 0, &root)) != 0)
		return (err);
	for (l = VM_LEVELS - 1; l >= 1; l--) {
		if ((err = tbl_alloc(vm, l, &t)) != 0)
			return (err);
		vm->nullt[l] = t;
	}
	for (i = 0; i < PAGE_SIZE / 8; i++)
		tbl_wr(vm, root, i, tbl_pde(vm, vm->nullt[1]));
	return (0);
}

int
amd_gmc12_vm_init(struct amd_gart *gt, struct amd_vm12 *vm, int mode,
    uint64_t flat_va)
{
	const struct hub_gen *g = hub_find(gt->sc, true);
	u_int root;

	memset(vm, 0, sizeof(*vm));
	vm->gt = gt;
	vm->mode = mode;
	vm->flat_va = flat_va;
	if (g == NULL)
		return (ENOTSUP);
	vm->translate_further = g->translate_further;
	if (!gt->enabled || rman_get_size(gt->vram) < VM_POOL_VRAM_OFF +
	    (bus_size_t)VM_POOL_PAGES * PAGE_SIZE)
		return (ENXIO);
	if (mode == AMD_VM12_4L_HEAP) {
		if (!gt->sc->vram.inited)
			return (ENXIO);
		vm->heap = malloc(AMD_VM12_HEAP_TABLES * sizeof(uint64_t),
		    M_DEVBUF, M_WAITOK | M_ZERO);
		sx_init(&vm->tlock, "atrium_vm12");
		vm->live = true;
		return (vm_init_null(vm));
	}
	/* A slice of the pool per live VM. */
	for (vm->slice = 0; vm->slice < VM_SLICES; vm->slice++)
		if ((gt->vm_slices & (1u << vm->slice)) == 0)
			break;
	if (vm->slice == VM_SLICES)
		return (ENOMEM);
	gt->vm_slices |= 1u << vm->slice;
	vm->first = vm->slice * VM_SLICE_PAGES;
	vm->live = true;
	if (mode == AMD_VM12_FLAT && (flat_va & ((2ull << 20) - 1)) != 0)
		return (EINVAL);
	return (tbl_alloc(vm, mode == AMD_VM12_FLAT ? VM_LEVELS - 1 : 0,
	    &root));	/* table 0 */
}

void
amd_gmc12_vm_fini(struct amd_vm12 *vm)
{
	u_int i;

	if (vm_sys(vm))
		for (i = 0; i < vm->used; i++)
			contigfree(vm->sys[i], PAGE_SIZE, M_DEVBUF);
	if (vm_heap(vm)) {
		/* The VMID points at nothing and was flushed: nothing walks
		 * these any more. The allocator scrubs them before reuse. */
		for (i = 0; i < vm->used; i++)
			amd_vram_free(vm->gt->sc, vm->heap[i], PAGE_SIZE);
		vm->used = 0;
		if (vm->live) {
			free(vm->heap, M_DEVBUF);
			vm->heap = NULL;
			sx_destroy(&vm->tlock);
		}
		vm->live = false;
		return;
	}
	vm->used = 0;
	if (vm->live)
		vm->gt->vm_slices &= ~(1u << vm->slice);
	vm->live = false;
}

/* Context base for this VM: table 0's address and flags. */
uint64_t
amd_gmc12_vm_pd_addr(struct amd_vm12 *vm)
{
	return (tbl_pde(vm, 0));
}

/* Walk to the PTE slot of `va`, allocating tables when `alloc`. */
static int
vm_walk(struct amd_vm12 *vm, uint64_t va, bool alloc, u_int *pt, u_int *pi)
{
	uint64_t pde;
	u_int lvl, shift, idx, t = 0, child;
	int err;

	if ((va & PAGE_MASK) != 0 || va >= (1ull << VM_VA_BITS))
		return (EINVAL);
	if (vm->mode == AMD_VM12_FLAT) {
		if (va < vm->flat_va || va - vm->flat_va >= (2ull << 20))
			return (EINVAL);
		*pt = 0;
		*pi = (u_int)((va - vm->flat_va) >> PAGE_SHIFT);
		return (0);
	}
	for (lvl = 0; lvl < VM_LEVELS - 1; lvl++) {
		shift = PAGE_SHIFT + 9 * (VM_LEVELS - 1 - lvl);
		idx = (va >> shift) & 511;
		pde = tbl_rd(vm, t, idx);
		if ((pde & PTE_VALID) == 0 || (tbl_of(vm, pde, &child) &&
		    tbl_null(vm, child))) {
			if (!alloc)
				return (ENOENT);
			err = tbl_alloc(vm, lvl + 1, &child);
			if (err != 0)
				return (err);
			pde = tbl_pde(vm, child);
			if (lvl == 1 && vm->translate_further)
				pde |= PDE_BFS(9);	/* an entry in a PDB1 */
			tbl_wr(vm, t, idx, pde);
		}
		if (!tbl_of(vm, pde, &t))
			return (EFAULT);
	}
	*pt = t;
	*pi = (va >> PAGE_SHIFT) & 511;
	return (0);
}

/* Map one system page. Caller flushes (amd_gmc12_vm_flush) afterwards. */
int
amd_gmc12_vm_map(struct amd_vm12 *vm, uint64_t va, vm_paddr_t pa,
    u_int prot)
{
	uint64_t pte;
	u_int t, i;
	int err;

	if ((pa & PAGE_MASK) != 0 || (pa & ~PTE_ADDR_MASK) != 0)
		return (EINVAL);
	if (vm_heap(vm))
		sx_xlock(&vm->tlock);
	if ((err = vm_walk(vm, va, true, &t, &i)) != 0) {
		if (vm_heap(vm))
			sx_xunlock(&vm->tlock);
		return (err);
	}
	/* Local VRAM: the physical FB address, neither SYSTEM nor SNOOPED
	 * (amdgpu_vm_bo_update for a VRAM BO; 4L: MTYPE NC). */
	pte = (uint64_t)pa | PTE_VALID;
	if ((prot & AMD_VM_VRAM) == 0)
		pte |= PTE_SYSTEM | PTE_SNOOPED;
	if (prot & AMD_VM_READ)
		pte |= PTE_READABLE;
	if (prot & AMD_VM_WRITE)
		pte |= PTE_WRITEABLE;
	if (prot & AMD_VM_EXEC)
		pte |= PTE_EXECUTABLE;
	if (vm->mode == AMD_VM12_FLAT)		/* as amd_gmc12_gart_map */
		pte |= PTE_MTYPE_UC;
	pte |= PTE_IS_PTE;			/* see above; 4L: MTYPE NC (0) */
	tbl_wr(vm, t, i, pte);
	if (vm_heap(vm))
		sx_xunlock(&vm->tlock);
	return (0);
}

/* Unmap one page: the invalid-PTE pattern. Caller flushes afterwards. */
int
amd_gmc12_vm_unmap(struct amd_vm12 *vm, uint64_t va)
{
	u_int t, i;
	int err;

	if (vm_heap(vm))
		sx_xlock(&vm->tlock);
	err = vm_walk(vm, va, false, &t, &i);
	if (err == 0)
		tbl_wr(vm, t, i, vm->mode == AMD_VM12_FLAT ? 0 :
		    PTE_EXECUTABLE | PTE_IS_PTE);
	if (vm_heap(vm))
		sx_xunlock(&vm->tlock);
	return (err);
}

/* Log the walk for `va`: each level's raw entry, read back. */
void
amd_gmc12_vm_dump(struct amd_vm12 *vm, uint64_t va)
{
	uint64_t e;
	u_int lvl, shift, idx, t = 0;
	static const char *names[] = { "PDB2", "PDB1", "PDB0", "PTB" };

	if (vm->mode == AMD_VM12_FLAT) {
		idx = (u_int)((va - vm->flat_va) >> PAGE_SHIFT) & 511;
		device_printf(vm->gt->sc->dev, "vm: walk %#jx: flat PTB[%u] = "
		    "%#018jx\n", (uintmax_t)va, idx,
		    (uintmax_t)tbl_rd(vm, 0, idx));
		return;
	}
	for (lvl = 0; lvl < VM_LEVELS; lvl++) {
		shift = PAGE_SHIFT + 9 * (VM_LEVELS - 1 - lvl);
		idx = (va >> shift) & 511;
		e = tbl_rd(vm, t, idx);
		device_printf(vm->gt->sc->dev, "vm: walk %#jx: %s (table %u at "
		    "%#jx)[%u] = %#018jx\n", (uintmax_t)va, names[lvl], t,
		    (uintmax_t)(tbl_pde(vm, t) & PDE_ADDR_MASK), idx,
		    (uintmax_t)e);
		if (lvl == VM_LEVELS - 1 || (e & PTE_VALID) == 0 ||
		    !tbl_of(vm, e, &t))
			break;
	}
}

/* HDP flush, then invalidate `vmid` on both hubs and wait for the acks. */
int
amd_gmc12_vm_flush(struct amd_gart *gt, u_int vmid)
{
	struct hub mm, gfx;
	int err = 0;

	if (vmid >= NUM_CTX)
		return (EINVAL);
	mtx_lock(&gmc12_inv_mtx);
	amd_hdp_flush(gt->sc);
	if (hub_open(gt->sc, false, &mm) && hub_flush(&mm, vmid) != 0) {
		err = ETIMEDOUT;
		hub_flush_report(&mm, vmid);
	}
	if (hub_open(gt->sc, true, &gfx) && hub_flush(&gfx, vmid) != 0) {
		err = ETIMEDOUT;
		hub_flush_report(&gfx, vmid);
	}
	mtx_unlock(&gmc12_inv_mtx);
	return (err);
}

/*
 * Point context `vmid` (1-7: the kernel's own; 8-15 belong to MES) at
 * `vm` on both hubs (depth, range and base) and flush it. vm NULL unbinds:
 * back to the setup_vmid_config state with base 0, so every access faults.
 */
int
amd_gmc12_vm_bind(struct amd_gart *gt, u_int vmid, struct amd_vm12 *vm)
{
	struct hub h;
	uint64_t base = 0, start = 0, end = VM_MAX_PFN - 1;
	u_int depth = VM_DEPTH;
	int i;

	if (vmid == 0 || vmid >= 8)
		return (EINVAL);
	if (vm != NULL) {
		base = amd_gmc12_vm_pd_addr(vm);
		if (vm->mode == AMD_VM12_FLAT) {
			depth = 0;
			start = vm->flat_va >> PAGE_SHIFT;
			end = start + 511;
		}
	}
	for (i = 0; i < 2; i++) {
		if (!hub_open(gt->sc, i == 1, &h))
			return (ENOTSUP);
		hrmw(&h, h.g->ctx0_cntl + vmid, CTX_DEPTH, depth << 1);
		hw(&h, h.g->ctx0_pt_start_lo + 2 * vmid, (uint32_t)start);
		hw(&h, h.g->ctx0_pt_start_hi + 2 * vmid,
		    (uint32_t)(start >> 32));
		hw(&h, h.g->ctx0_pt_end_lo + 2 * vmid, (uint32_t)end);
		hw(&h, h.g->ctx0_pt_end_hi + 2 * vmid, (uint32_t)(end >> 32));
		hw(&h, h.g->ctx0_pt_base_lo + 2 * vmid, (uint32_t)base);
		hw(&h, h.g->ctx0_pt_base_hi + 2 * vmid, (uint32_t)(base >> 32));
	}
	return (amd_gmc12_vm_flush(gt, vmid));
}

/* GFXHUB's context `vmid` registers and the L2 setup, read back. */
void
amd_gmc12_ctx_report(struct amd_gart *gt, u_int vmid, const char *when)
{
	struct hub h;
	const struct hub_gen *g;

	if (vmid >= NUM_CTX || !hub_open(gt->sc, true, &h))
		return;
	g = h.g;
	device_printf(gt->sc->dev, "gmc: %s %s: CONTEXT%u_CNTL %#x, base "
	    "%#x%08x, start %#x%08x, end %#x%08x; L2_CNTL %#x, L2_CNTL3 %#x, "
	    "L2_CNTL4 %#x, L2_CNTL5 %#x, FAULT_CNTL %#x, FAULT_CNTL2 %#x\n",
	    g->name, when, vmid, hr(&h, g->ctx0_cntl + vmid),
	    hr(&h, g->ctx0_pt_base_hi + 2 * vmid),
	    hr(&h, g->ctx0_pt_base_lo + 2 * vmid),
	    hr(&h, g->ctx0_pt_start_hi + 2 * vmid),
	    hr(&h, g->ctx0_pt_start_lo + 2 * vmid),
	    hr(&h, g->ctx0_pt_end_hi + 2 * vmid),
	    hr(&h, g->ctx0_pt_end_lo + 2 * vmid), hr(&h, g->l2_cntl),
	    hr(&h, g->l2_cntl3), hr(&h, g->l2_cntl4), hr(&h, g->l2_cntl5),
	    hr(&h, g->fault_cntl), hr(&h, g->fault_cntl2));
}

/*
 * GFXHUB's latched protection fault (status, page address), then clear
 * it for the next one (gmc_v12_0_process_interrupt). Returns false when
 * nothing is latched.
 */
bool
amd_gmc12_fault_take(struct amd_gart *gt, uint32_t *status, uint64_t *addr)
{
	struct hub h;

	if (!hub_open(gt->sc, true, &h))
		return (false);
	*status = hr(&h, h.g->fault_status);
	*addr = ((uint64_t)(hr(&h, h.g->fault_addr_hi) & 0xf) << 32 |
	    hr(&h, h.g->fault_addr_lo)) << PAGE_SHIFT;
	hrmw(&h, h.g->fault_cntl, 0, FAULT_CNTL_CLEAR);
	hrmw(&h, h.g->fault_cntl, FAULT_CNTL_CLEAR, 0);
	return (*status != 0);
}

/* gfxhub_v12_0_print_l2_protection_fault_status, one line. */
void
amd_gmc12_fault_decode(struct amd_gart *gt, uint32_t st, const char *when)
{
	static const char *cids[] = { "CB", "DB", "GE1", "GE2", "CPF", "CPC",
	    "CPG", "RLC", "TCP", "SQC (inst)", "SQC (data)", "SQG/PC/SC",
	    "Reserved", "SDMA0", "SDMA1", "GCR", "Reserved", "Reserved",
	    "WGS", "DSM", "PA" };
	u_int cid = FAULT_STATUS_CID(st);

	device_printf(gt->sc->dev, "gmc: %s: fault status %#x: client %s "
	    "(%#x), VMID %u, MORE_FAULTS %u, WALKER_ERROR %#x, "
	    "PERMISSION_FAULTS %#x, MAPPING_ERROR %u, RW %u, ATOMIC %u, "
	    "PRT %u, UCE %u\n", when, st, cid < nitems(cids) ? cids[cid] :
	    "unknown", cid, FAULT_STATUS_VMID(st), st & 1, (st >> 1) & 7,
	    (st >> 4) & 0xf, (st >> 8) & 1, (st >> 18) & 1, (st >> 19) & 1,
	    (st >> 30) & 1, st >> 31);
}

u_int
amd_gmc12_fault_vmid(uint32_t status)
{
	return (FAULT_STATUS_VMID(status));
}

/* GFXHUB CONTEXTn_CNTL as programmed (MES ADD_QUEUE's vm_context_cntl). */
uint32_t
amd_gmc12_ctx_cntl(struct amd_gart *gt, u_int vmid)
{
	struct hub h;

	if (vmid >= NUM_CTX || !hub_open(gt->sc, true, &h))
		return (0);
	return (hr(&h, h.g->ctx0_cntl + vmid));
}
