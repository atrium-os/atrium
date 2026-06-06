/*
 * atrium-gpu-amd — from-scratch FreeBSD kernel driver for AMD RDNA4 GPUs.
 *
 * KERNEL side of the Atrium GPU split (kernel = C, userspace = Rust),
 * exercised against the gpusim functional model (vendor 0x1002 / device
 * 0x7550) before real silicon. Milestones (design §8):
 *
 *   M1  newbus PCI bring-up + BAR mapping + first register read (GRBM_STATUS
 *       over PCI), gated behind COMMAND.MSE/BME.
 *   M2  bring the GPU "alive": reset to a known state, load CP microcode
 *       (the PSP firmware load), and initialize the MES scheduler — the
 *       handshake the model's referee requires before any queue/doorbell
 *       can be honored (device-reference §4 steps 2 + 5).
 *   M3  first submit (proof of life): bring up GMC/IH, build a 2-level
 *       GPUVM page table, lay a PM4 ring [NOP, RELEASE_MEM], map queue 0,
 *       and ring the doorbell — the CP DMA-walks the page tables, fetches +
 *       executes the ring, and DMA-writes a fence we read back. The first
 *       *positive* confirmation (steps 3,4,6,7) the whole stack is alive.
 *   M4  real compute: dispatch a built-in INC kernel on a second queue via
 *       the MEC HQD path and read back results that depend on the input
 *       (dst[i] = src[i]+1) — proof the GPU does work, not just drains rings.
 *
 * WHY one combined file here, not the §4.2 gmc.c/cp.c/bo.c split (or the
 * §4.1 three-kmod split): through M3 the driver is a single coherent
 * "cold → alive → first submit" story that reads best in one place. The
 * per-block files earn their keep once each grows real surface area —
 * multiple engines/queues, BO lifecycle, eviction, an ioctl ABI; doing it
 * now would be structure without a reason for it (the very thing §2
 * rejects). Split when a block needs it.
 */

#include <sys/param.h>
#include <sys/module.h>
#include <sys/kernel.h>
#include <sys/systm.h>
#include <sys/bus.h>
#include <sys/malloc.h>
#include <sys/rman.h>

#include <machine/bus.h>
#include <machine/resource.h>

#include <vm/vm.h>
#include <vm/pmap.h>

#include <dev/pci/pcivar.h>
#include <dev/pci/pcireg.h>

#define ATRIUM_AMD_VENDOR	0x1002
#define ATRIUM_AMD_DEVICE	0x7550	/* RDNA4-class (gpusim target) */

/*
 * BAR5 = the MMIO register file, 256 KiB, 32-bit mem (gpusim device-
 * reference §2). PCIR_BAR(5) is its config-space BAR offset == the rid
 * bus_alloc_resource wants.
 */
#define ATRIUM_AMD_REGS_BAR	PCIR_BAR(5)

/*
 * Register offsets within BAR5. A real GFX12 register's absolute BAR5
 * offset = APER_<block> + reg_dword_offset*4 (device-reference §3).
 *
 * GRBM_STATUS: GC aperture base 0x0_0000 + dword 0x0da4 * 4 = 0x3690.
 * (gc_12_0_0 header; the value/field encodings are modeled functionally,
 *  the offset is bit-exact.)
 */
#define regGRBM_STATUS		0x3690

/*
 * gpusim identity probe register. Per the model (engine/src/device.rs
 * `regs::ID`) this lives at ABSOLUTE BAR5 offset 0x00 — note it is NOT in
 * the SIM aperture despite the device-reference prose; the const is a raw
 * 0x00, not sim(0x00). Reading it returns GPUSIM_MAGIC ('GPUS', stored
 * MSB-first as 0x47505553). Confirms (a) we're talking to the model and
 * (b) Memory-Space-Enable took effect — before MSE the referee leaves BAR
 * reads unclaimed (all-ones), INV-PCI-0002.
 */
#define regSIM_ID		0x00
#define SIM_ID_MAGIC		0x47505553u	/* 'G','P','U','S' (model GPUSIM_MAGIC) */

/*
 * SIM-aperture bring-up registers (device-reference §4; model
 * engine/src/device.rs `regs`). These have no single real-HW analog — they
 * model the PSP/MES/reset handshake a real driver drives through a dozen
 * scattered GC/SMU registers. The model places them at APER_SIM (BAR5
 * 0x2_0000) + the documented offset; absolute offsets are spelled out so a
 * reader can match them to the model without arithmetic.
 *
 * Reset handshake (mode-1/FLR class): write REQ=1, poll STATUS until it
 * reads 1 (reset latched, awaiting ack), write ACK=1 (STATUS clears). A full
 * reset wipes engine state, so CP firmware must be (re)loaded afterwards.
 *
 * Firmware: stage FW_CP_VERSION (the ucode the PSP would load), then write
 * FW_CP_LOAD=1 — the model activates the CP only if the staged version is at
 * least CP_FW_MIN_VERSION (an older ucode is refused, mirroring a real
 * firmware-too-old failure). MES_INIT=1 then brings the scheduler up, and is
 * gated on the CP firmware being loaded.
 */
#define ATRIUM_AMD_APER_SIM	0x20000		/* BAR5 SIM-aperture base */
#define regFW_CP_VERSION	(ATRIUM_AMD_APER_SIM + 0x50)	/* staged CP ucode version (w) */
#define regFW_CP_LOAD		(ATRIUM_AMD_APER_SIM + 0x54)	/* w 1: validate + activate CP */
#define regRESET_REQ		(ATRIUM_AMD_APER_SIM + 0x58)	/* w 1: begin full GPU reset */
#define regRESET_STATUS		(ATRIUM_AMD_APER_SIM + 0x5c)	/* r 1: reset latched, awaiting ack */
#define regRESET_ACK		(ATRIUM_AMD_APER_SIM + 0x60)	/* w 1: ack / close reset window */
#define regMES_INIT		(ATRIUM_AMD_APER_SIM + 0x68)	/* w 1: init MES (needs CP fw) */

/*
 * Minimum CP microcode version the model accepts (engine/src/device.rs
 * CP_FW_MIN_VERSION). A real driver derives this from the firmware blob it
 * loads from disk; the model has no blob, so we stage exactly the minimum.
 */
#define ATRIUM_AMD_CP_FW_VERSION	0x40

/*
 * Bounded poll for the reset to latch. The model completes the reset
 * synchronously (STATUS reads 1 on the first poll), but real silicon takes
 * microseconds — so we poll with a timeout rather than assume instant, which
 * is the shape the driver must keep for hardware.
 */
#define ATRIUM_AMD_RESET_POLLS	1000		/* max polls (×10us = 10ms budget) */
#define ATRIUM_AMD_RESET_DELAY	10		/* us between polls */

/*
 * Milestone-3 registers (real GFX12 offsets; gc()=GC aperture dword*4,
 * oss()=OSS aperture base 0x1_0000 + dword*4, per device-reference §3 and
 * engine/src/device.rs `regs`). GMC programs the GPUVM page-directory base
 * for VMID 0 and enables paging; IH stands up the interrupt ring; the CP_RB0
 * block maps the graphics ring (queue 0).
 */
#define regGMC_ENABLE		0x5890	/* GCVM_CONTEXT0_CNTL: enable paging */
#define regPT_BASE_LO		0x5a3c	/* GCVM_CONTEXT0_PAGE_TABLE_BASE_ADDR_LO32 */
#define regPT_BASE_HI		0x5a40	/* ..._HI32 */
#define regTLB_INVALIDATE	0x591c	/* GCVM_INVALIDATE_ENG0_REQ: flush GPU TLB */
#define regIH_SIZE		0x10200	/* IH_RB_CNTL (ring size in entries) */
#define regIH_BASE_LO		0x1020c	/* IH_RB_BASE */
#define regIH_BASE_HI		0x10210	/* IH_RB_BASE_HI */
#define regCP_RB0_BASE		0x7780	/* gfx ring base (register holds base>>8) */
#define regCP_RB0_CNTL		0x7784	/* gfx ring size (bytes) */
#define regCP_RB_DOORBELL_CONTROL 0x7a34	/* gfx ring doorbell offset (in BAR2) */
#define regCP_ME_CNTL		0x200c	/* write 0: clear ME halt -> run the ring */

/*
 * GPUVM page-table entry valid bit (residency.rs PTE_VALID). The GMC
 * DMA-walks a 2-level table: VA[29:21] indexes the page directory, VA[20:12]
 * the page table; each entry is 8 bytes = (page-aligned phys) | PTE_VALID.
 */
#define ATRIUM_AMD_PTE_VALID	0x1ULL
#define ATRIUM_AMD_PD_SHIFT	21	/* directory index = (va >> 21) & 0x1ff */
#define ATRIUM_AMD_PT_SHIFT	12	/* table index     = (va >> 12) & 0x1ff */
#define ATRIUM_AMD_PT_MASK	0x1ff

/*
 * PM4 type-3 packet encoding (engine/src/pm4.rs; header layout is public
 * documented PM4, IT_ opcodes verified vs kfd_pm4_opcodes.h). We emit just
 * the two packets the proof-of-life ring needs: a NOP and a RELEASE_MEM that
 * DMA-writes a 64-bit fence and raises an end-of-pipe interrupt.
 */
#define PM4_TYPE3		3u
#define IT_NOP			0x10u
#define IT_RELEASE_MEM		0x49u
/* RELEASE_MEM body field shifts/values (soc15d.h-derived). */
#define RM_EVENT_INDEX_EOP_AT8	(5u << 8)	/* DWORD1: EVENT_INDEX = end-of-pipe */
#define RM_DATA_SEL_64BIT_AT29	(2u << 29)	/* DWORD2: write a 64-bit value */
#define RM_INT_SEL_ON_CONFIRM_AT24 (2u << 24)	/* DWORD2: IRQ when the write confirms */

/* Ring/queue sizing: one page each; queue 0 doorbell lives at BAR2 offset 0. */
#define ATRIUM_AMD_RING_BYTES	256	/* CP_RB0_CNTL value (matches reference) */
#define ATRIUM_AMD_DOORBELL_OFF	0	/* queue 0 doorbell offset within BAR2 */

/* GPU-VAs for the proof-of-life buffers (arbitrary, page-aligned, same PD). */
#define ATRIUM_AMD_RING_VA	0x200000ULL
#define ATRIUM_AMD_FENCE_VA	0x201000ULL
/* A recognizable 64-bit fence value; both halves nonzero to exercise DATA_SEL_64BIT. */
#define ATRIUM_AMD_FENCE_MAGIC	0xcafef00ddeadbeefULL

/* Doorbell BAR (BAR2): 64-bit MMIO doorbell page (device-reference §5). */
#define ATRIUM_AMD_DOORBELL_BAR	PCIR_BAR(2)

/* Small fixed registry of DMA pages (page tables + ring + fence + IH). */
#define ATRIUM_AMD_MAX_DMA	16

/*
 * Milestone-4 compute path: a DISPATCH runs a built-in kernel element-wise
 * over a buffer (engine/src/render.rs SoftwareBackend). The compute queue is
 * mapped via the MEC HQD registers (CP_HQD_*, real GFX12 offsets) rather than
 * the gfx CP_RB0_* path; HQD_SELECT/COMPUTE_* live in the SIM aperture.
 */
#define regHQD_SELECT			0x20080	/* which HQD the CP_HQD regs program */
#define regCP_HQD_PQ_BASE		0x7ec4	/* HQD ring base (holds base>>8) */
#define regCP_HQD_PQ_CONTROL		0x7ee8	/* HQD ring size */
#define regCP_HQD_PQ_DOORBELL_CONTROL	0x7ee0	/* HQD doorbell offset (in BAR2) */
#define regCP_HQD_ACTIVE		0x7eac	/* write 1: activate (MQD->HQD, gated on MES) */
#define regCOMPUTE_KERNEL		0x20200	/* built-in kernel selector */
#define regCOMPUTE_SRC_LO		0x20204	/* source buffer GPU-VA (lo/hi) */
#define regCOMPUTE_SRC_HI		0x20208
#define regCOMPUTE_DST_LO		0x2020c	/* dest buffer GPU-VA (lo/hi) */
#define regCOMPUTE_DST_HI		0x20210

#define IT_DISPATCH_DIRECT		0x15u	/* PM4 compute dispatch */
#define KERNEL_INC			2u	/* dst[i] = src[i] + 1 (render.rs) */

/* Compute job: queue 1 (independent of run_job's queue 0), doorbell 0x8. */
#define ATRIUM_AMD_COMPUTE_QID		1
#define ATRIUM_AMD_COMPUTE_DOORBELL	0x8
#define ATRIUM_AMD_COMPUTE_SRC_VA	0x300000ULL
#define ATRIUM_AMD_COMPUTE_DST_VA	0x301000ULL
#define ATRIUM_AMD_COMPUTE_RING_VA	0x302000ULL
#define ATRIUM_AMD_COMPUTE_N		4	/* elements (one page holds plenty) */

/*
 * A page of DMA-able guest memory: its kernel virtual address (where the CPU
 * reads/writes) and its guest-physical address (what the device DMA-walks).
 * In a VM with no IOMMU on this device, gpa == vtophys(kva) — see amd_dma_alloc.
 */
struct atrium_amd_dma_page {
	void		*kva;
	vm_paddr_t	 gpa;
};

struct atrium_amd_softc {
	device_t	 dev;
	struct resource	*regs;		/* BAR5 MMIO register file */
	int		 regs_rid;
	struct resource	*doorbell;	/* BAR2 doorbell page */
	int		 doorbell_rid;

	void		*pdb_kva;	/* GPUVM page-directory base (VMID 0) */
	vm_paddr_t	 pdb_gpa;

	struct atrium_amd_dma_page dma[ATRIUM_AMD_MAX_DMA];
	int		 n_dma;
};

/*
 * Leaf MMIO mechanic (ring_helpers.c tier — §3.3 / §7.1). The ONLY place
 * raw bus_space register access lives; inline here while the driver is one
 * file, lifts to ring_helpers.c when there's a second user. Chip-agnostic:
 * takes a byte offset into the REGS BAR, touches no driver state, no control
 * flow. Callers read as `amd_mmio_read32(sc, regFOO)` and annotate the
 * register's meaning at the call site (§7.1).
 *
 * WHY a leaf helper, not bare bus_read_4/write_4 at each site: §7.1 —
 * register access is a leaf, so the BAR handle (and any future ordering/
 * trace policy) lives in one spot, not scattered across every caller. The
 * write form lands here at milestone 2, when the reset/firmware/MES
 * handshake first writes registers (§7.5: no dead code before then).
 */
static inline uint32_t
amd_mmio_read32(struct atrium_amd_softc *sc, bus_size_t reg)
{
	return (bus_read_4(sc->regs, reg));
}

static inline void
amd_mmio_write32(struct atrium_amd_softc *sc, bus_size_t reg, uint32_t val)
{
	bus_write_4(sc->regs, reg, val);
}

/*
 * PM4 type-3 header (leaf, §3.3 — pure encoding, no state/control flow):
 * type[31:30] | (body_dwords-1)[29:16] | opcode[15:8]. Public PM4 layout.
 */
static inline uint32_t
amd_pm4_type3_header(uint32_t opcode, uint32_t body_dwords)
{
	return ((PM4_TYPE3 << 30) | (((body_dwords - 1) & 0x3fff) << 16) |
	    (opcode << 8));
}

/*
 * Allocate one page of DMA-able guest memory and register it. Returns the
 * kernel VA (CPU side) and, via *gpa_out, the guest-physical address the
 * device DMA-walks.
 *
 * WHY contigmalloc + vtophys, not busdma here: gpusim runs in a VM with no
 * IOMMU on this device, so the guest-physical address IS the address the
 * model's DMA backend (QEMU pci_dma_read/write) uses — vtophys() of a
 * single page-aligned page gives exactly that. Real silicon will need
 * bus_dma tags + bus_dmamap_sync (IOMMU translation + cache maintenance);
 * that lands with the BO allocator (bo.c, §4.2), not in this proof-of-life.
 */
static void *
amd_dma_alloc(struct atrium_amd_softc *sc, vm_paddr_t *gpa_out)
{
	void *kva;

	if (sc->n_dma >= ATRIUM_AMD_MAX_DMA)
		return (NULL);
	kva = contigmalloc(PAGE_SIZE, M_DEVBUF, M_WAITOK | M_ZERO, 0,
	    BUS_SPACE_MAXADDR, PAGE_SIZE, 0);
	if (kva == NULL)
		return (NULL);
	sc->dma[sc->n_dma].kva = kva;
	sc->dma[sc->n_dma].gpa = vtophys(kva);
	*gpa_out = sc->dma[sc->n_dma].gpa;
	sc->n_dma++;
	return (kva);
}

/*
 * Map a registered DMA page's guest-physical address back to its kernel VA —
 * needed when the GPUVM walker reuses a page-table page we already allocated
 * (we hold the PDE's phys, but must write PTEs through the CPU mapping).
 */
static void *
amd_dma_kva(struct atrium_amd_softc *sc, vm_paddr_t gpa)
{
	int i;

	for (i = 0; i < sc->n_dma; i++)
		if (sc->dma[i].gpa == gpa)
			return (sc->dma[i].kva);
	return (NULL);
}

/*
 * Edit the 2-level GPUVM page table to map GPU-VA `va` -> guest-physical
 * `phys`, allocating a page-table page on demand, then flush the GPU TLB so
 * the device does not walk a stale cached translation (referee INV-GMC-0004).
 * Mirrors the model's walk (residency.rs): PDE at pdb[va>>21], PTE at
 * pt[va>>12], each = (page-aligned phys) | PTE_VALID.
 */
static int
amd_gpuvm_map(struct atrium_amd_softc *sc, uint64_t va, vm_paddr_t phys)
{
	uint64_t *pde, *pt_kva;
	vm_paddr_t pt_gpa;
	uint32_t pd_i, pt_i;

	pd_i = (va >> ATRIUM_AMD_PD_SHIFT) & ATRIUM_AMD_PT_MASK;
	pt_i = (va >> ATRIUM_AMD_PT_SHIFT) & ATRIUM_AMD_PT_MASK;
	pde = (uint64_t *)((char *)sc->pdb_kva + pd_i * 8);

	if ((*pde & ATRIUM_AMD_PTE_VALID) == 0) {
		pt_kva = amd_dma_alloc(sc, &pt_gpa);
		if (pt_kva == NULL)
			return (ENOMEM);
		*pde = (pt_gpa & ~0xfffULL) | ATRIUM_AMD_PTE_VALID;
	} else {
		pt_gpa = *pde & ~0xfffULL;
		pt_kva = amd_dma_kva(sc, pt_gpa);
		if (pt_kva == NULL)
			return (ENXIO);
	}
	pt_kva[pt_i] = ((uint64_t)phys & ~0xfffULL) | ATRIUM_AMD_PTE_VALID;

	amd_mmio_write32(sc, regTLB_INVALIDATE, 1);
	return (0);
}

/*
 * Proof-of-life submit (the milestone-3 deliverable): map a ring + a fence
 * buffer, lay a PM4 ring [NOP, RELEASE_MEM(fence, irq)], map queue 0 onto the
 * gfx ring, and ring its doorbell. The model's CP DMA-fetches the ring,
 * executes it, and DMA-writes the 64-bit fence value back into our buffer.
 * Reading that value back == positive proof the whole stack is alive (without
 * firmware+MES the doorbell would leave the ring undrained — referee
 * doorbell_before_firmware / INV-QUEUE-0001). Returns the fence read back.
 */
static uint64_t
amd_submit_runjob(struct atrium_amd_softc *sc)
{
	void *ring_kva, *fence_kva;
	vm_paddr_t ring_gpa, fence_gpa;
	volatile uint64_t *fence;
	uint32_t *r;
	int n;

	ring_kva = amd_dma_alloc(sc, &ring_gpa);
	fence_kva = amd_dma_alloc(sc, &fence_gpa);
	if (ring_kva == NULL || fence_kva == NULL)
		return (0);
	if (amd_gpuvm_map(sc, ATRIUM_AMD_RING_VA, ring_gpa) != 0 ||
	    amd_gpuvm_map(sc, ATRIUM_AMD_FENCE_VA, fence_gpa) != 0)
		return (0);

	/* Lay the ring (9 dwords). NOP{count=1} = header + 1 pad dword. */
	r = (uint32_t *)ring_kva;
	n = 0;
	r[n++] = amd_pm4_type3_header(IT_NOP, 1);
	r[n++] = 0;
	/* RELEASE_MEM: header + 6 body (event ctl, data/int ctl, addr, value). */
	r[n++] = amd_pm4_type3_header(IT_RELEASE_MEM, 6);
	r[n++] = RM_EVENT_INDEX_EOP_AT8;
	r[n++] = RM_DATA_SEL_64BIT_AT29 | RM_INT_SEL_ON_CONFIRM_AT24;
	r[n++] = (uint32_t)(ATRIUM_AMD_FENCE_VA & 0xffffffff);
	r[n++] = (uint32_t)(ATRIUM_AMD_FENCE_VA >> 32);
	r[n++] = (uint32_t)(ATRIUM_AMD_FENCE_MAGIC & 0xffffffff);
	r[n++] = (uint32_t)(ATRIUM_AMD_FENCE_MAGIC >> 32);

	/*
	 * Map queue 0 onto the gfx ring via the real CP_RB0_* registers (ring
	 * base holds VA>>8), point its doorbell at BAR2 offset 0, then clear
	 * the ME halt so the CP will run the ring once doorbelled.
	 */
	amd_mmio_write32(sc, regCP_RB0_BASE, (uint32_t)(ATRIUM_AMD_RING_VA >> 8));
	amd_mmio_write32(sc, regCP_RB0_CNTL, ATRIUM_AMD_RING_BYTES);
	amd_mmio_write32(sc, regCP_RB_DOORBELL_CONTROL, ATRIUM_AMD_DOORBELL_OFF);
	amd_mmio_write32(sc, regCP_ME_CNTL, 0);

	/*
	 * Ring the doorbell: write the new write-pointer (in dwords) to the
	 * queue's doorbell in BAR2. The model drains the ring synchronously
	 * within this MMIO write — the fence is in guest RAM before it returns.
	 * (In a VM the MMIO trap serializes our prior ring/PTE stores ahead of
	 * the device's DMA read; real HW adds bus_dmamap_sync.)
	 */
	bus_write_4(sc->doorbell, ATRIUM_AMD_DOORBELL_OFF, n);

	fence = (volatile uint64_t *)fence_kva;
	return (*fence);
}

/*
 * Compute dispatch (milestone 4): run a built-in INC kernel over a small
 * buffer on a second, independent queue mapped via the MEC HQD path, and read
 * back the results. Where run_job proved "a ring drains," this proves the GPU
 * does real WORK whose output depends on the input: dst[i] = src[i] + 1,
 * reached through GPUVM (the SoftwareBackend DMA-walks src/dst under VMID 0).
 */
static void
amd_dispatch_compute(struct atrium_amd_softc *sc)
{
	void *src_kva, *dst_kva, *ring_kva;
	vm_paddr_t src_gpa, dst_gpa, ring_gpa;
	volatile uint32_t *dst;
	uint32_t *src, *r;
	uint32_t input[ATRIUM_AMD_COMPUTE_N] = { 10, 20, 30, 40 };
	int i, n, ok;

	src_kva = amd_dma_alloc(sc, &src_gpa);
	dst_kva = amd_dma_alloc(sc, &dst_gpa);
	ring_kva = amd_dma_alloc(sc, &ring_gpa);
	if (src_kva == NULL || dst_kva == NULL || ring_kva == NULL) {
		device_printf(sc->dev, "compute: out of DMA pages\n");
		return;
	}
	if (amd_gpuvm_map(sc, ATRIUM_AMD_COMPUTE_SRC_VA, src_gpa) != 0 ||
	    amd_gpuvm_map(sc, ATRIUM_AMD_COMPUTE_DST_VA, dst_gpa) != 0 ||
	    amd_gpuvm_map(sc, ATRIUM_AMD_COMPUTE_RING_VA, ring_gpa) != 0) {
		device_printf(sc->dev, "compute: failed to map buffers\n");
		return;
	}

	/* Stage the input array into the source buffer. */
	src = (uint32_t *)src_kva;
	for (i = 0; i < ATRIUM_AMD_COMPUTE_N; i++)
		src[i] = input[i];

	/*
	 * Compute state: the kernel selector + source/dest GPU-VAs. The
	 * SoftwareBackend reads src and writes dst by walking these VAs through
	 * GPUVM, so they must be mapped (above) before the dispatch runs.
	 */
	amd_mmio_write32(sc, regCOMPUTE_KERNEL, KERNEL_INC);
	amd_mmio_write32(sc, regCOMPUTE_SRC_LO,
	    (uint32_t)(ATRIUM_AMD_COMPUTE_SRC_VA & 0xffffffff));
	amd_mmio_write32(sc, regCOMPUTE_SRC_HI,
	    (uint32_t)(ATRIUM_AMD_COMPUTE_SRC_VA >> 32));
	amd_mmio_write32(sc, regCOMPUTE_DST_LO,
	    (uint32_t)(ATRIUM_AMD_COMPUTE_DST_VA & 0xffffffff));
	amd_mmio_write32(sc, regCOMPUTE_DST_HI,
	    (uint32_t)(ATRIUM_AMD_COMPUTE_DST_VA >> 32));

	/* Lay the DISPATCH ring: one packet, 3 body dwords (x=count, y, z). */
	r = (uint32_t *)ring_kva;
	n = 0;
	r[n++] = amd_pm4_type3_header(IT_DISPATCH_DIRECT, 3);
	r[n++] = ATRIUM_AMD_COMPUTE_N;	/* x = element count */
	r[n++] = 1;			/* y */
	r[n++] = 1;			/* z */

	/*
	 * Map the compute queue onto an HQD via the MEC CP_HQD_* registers
	 * (ring base holds VA>>8), point its doorbell at BAR2 offset 0x8, and
	 * activate — the MQD->HQD map the MES performs, gated on MES init.
	 */
	amd_mmio_write32(sc, regHQD_SELECT, ATRIUM_AMD_COMPUTE_QID);
	amd_mmio_write32(sc, regCP_HQD_PQ_BASE,
	    (uint32_t)(ATRIUM_AMD_COMPUTE_RING_VA >> 8));
	amd_mmio_write32(sc, regCP_HQD_PQ_CONTROL, ATRIUM_AMD_RING_BYTES);
	amd_mmio_write32(sc, regCP_HQD_PQ_DOORBELL_CONTROL,
	    ATRIUM_AMD_COMPUTE_DOORBELL);
	amd_mmio_write32(sc, regCP_HQD_ACTIVE, 1);

	/* Ring queue 1's doorbell; the model runs the kernel synchronously. */
	bus_write_4(sc->doorbell, ATRIUM_AMD_COMPUTE_DOORBELL, n);

	dst = (volatile uint32_t *)dst_kva;
	ok = 1;
	for (i = 0; i < ATRIUM_AMD_COMPUTE_N; i++)
		if (dst[i] != input[i] + 1)
			ok = 0;
	if (ok)
		device_printf(sc->dev, "compute OK: INC [%u %u %u %u] -> "
		    "[%u %u %u %u]\n", input[0], input[1], input[2], input[3],
		    dst[0], dst[1], dst[2], dst[3]);
	else
		device_printf(sc->dev, "compute FAILED: got [%u %u %u %u]\n",
		    dst[0], dst[1], dst[2], dst[3]);
}

/*
 * Release everything attach acquired: the DMA pages (page tables, ring,
 * fence, IH) and the two BAR resources. Safe to call partway through a failed
 * attach — every field is NULL/zero until its step runs. Used by both the
 * attach error paths and detach (sc->dev is set before anything is acquired).
 */
static void
amd_teardown(struct atrium_amd_softc *sc)
{
	int i;

	for (i = 0; i < sc->n_dma; i++)
		free(sc->dma[i].kva, M_DEVBUF);
	sc->n_dma = 0;
	sc->pdb_kva = NULL;
	if (sc->doorbell != NULL) {
		bus_release_resource(sc->dev, SYS_RES_MEMORY,
		    sc->doorbell_rid, sc->doorbell);
		sc->doorbell = NULL;
	}
	if (sc->regs != NULL) {
		bus_release_resource(sc->dev, SYS_RES_MEMORY, sc->regs_rid,
		    sc->regs);
		sc->regs = NULL;
	}
}

static int
atrium_amd_probe(device_t dev)
{
	if (pci_get_vendor(dev) == ATRIUM_AMD_VENDOR &&
	    pci_get_device(dev) == ATRIUM_AMD_DEVICE) {
		device_set_desc(dev, "Atrium AMD RDNA4 GPU (gpusim)");
		return (BUS_PROBE_DEFAULT);
	}
	return (ENXIO);
}

static int
atrium_amd_attach(device_t dev)
{
	struct atrium_amd_softc *sc = device_get_softc(dev);
	uint32_t id, grbm;
	vm_paddr_t ih_gpa;
	uint64_t fence;
	int i;

	sc->dev = dev;

	/*
	 * PCI bring-up gate (device-reference §2, §4; referee INV-PCI-0001/
	 * 0002): the device faults DMA before Bus-Master-Enable and BAR
	 * access before Memory-Space-Enable. pci_enable_busmaster() sets BME;
	 * allocating the BAR below with RF_ACTIVE enables memory-space
	 * decoding (MSE). COMMAND writes are mirrored to the model, which then
	 * unlocks BAR5 — so this ordering is load-bearing, not cosmetic.
	 */
	pci_enable_busmaster(dev);

	sc->regs_rid = ATRIUM_AMD_REGS_BAR;
	sc->regs = bus_alloc_resource_any(dev, SYS_RES_MEMORY, &sc->regs_rid,
	    RF_ACTIVE);
	if (sc->regs == NULL) {
		device_printf(dev, "failed to map BAR5 (MMIO register file)\n");
		return (ENXIO);
	}

	/* BAR2 = the doorbell page (device-reference §5); queue 0 rings here. */
	sc->doorbell_rid = ATRIUM_AMD_DOORBELL_BAR;
	sc->doorbell = bus_alloc_resource_any(dev, SYS_RES_MEMORY,
	    &sc->doorbell_rid, RF_ACTIVE);
	if (sc->doorbell == NULL) {
		device_printf(dev, "failed to map BAR2 (doorbell)\n");
		bus_release_resource(dev, SYS_RES_MEMORY, sc->regs_rid,
		    sc->regs);
		return (ENXIO);
	}

	/*
	 * Sanity: the SIM-aperture ID register reads magic 'GPUS'. If MSE
	 * had not taken effect this would read 0xffffffff (the referee
	 * leaving the access unclaimed) — so this doubles as a bring-up check.
	 */
	id = amd_mmio_read32(sc, regSIM_ID);
	device_printf(dev, "identity probe = 0x%08x ('%c%c%c%c')%s\n", id,
	    (int)((id >> 24) & 0xff), (int)((id >> 16) & 0xff),
	    (int)((id >> 8) & 0xff), (int)(id & 0xff),
	    id == SIM_ID_MAGIC ? " OK" : " [UNEXPECTED — not 'GPUS']");

	/*
	 * Milestone 1 (design §8): read GRBM_STATUS over PCI and print it.
	 * Against gpusim this reads 0 — the model is FUNCTIONAL, not register-
	 * complete; reg_read only implements the registers the bring-up
	 * handshake consumes, returning 0 for the rest (GRBM_STATUS among
	 * them). On real Navi 48 silicon this is non-zero (idle/busy state).
	 */
	grbm = amd_mmio_read32(sc, regGRBM_STATUS);
	device_printf(dev, "GRBM_STATUS = 0x%08x (gpusim models this as 0)\n",
	    grbm);

	/*
	 * Milestones 2-3 (design §8): walk the device cold -> alive -> first
	 * submit. The order is load-bearing and the model's referee enforces
	 * it: a full reset tears down the CP, so firmware reloads after it; the
	 * MES rides on the CP microcode (firmware before MES); and a doorbell
	 * before firmware, or a queue map before MES, faults. GMC/IH come up
	 * first so the CP has page tables + an interrupt ring to use.
	 *
	 * Step 1 — reset to a known state. A from-scratch driver inherits the
	 * device in whatever state firmware/a prior driver left it; reset to
	 * a defined baseline before programming. RESET_STATUS is the one
	 * bring-up status the model exposes readably, so it doubles as our
	 * verification that SIM-aperture writes land and read back.
	 */
	amd_mmio_write32(sc, regRESET_REQ, 1);
	for (i = 0; i < ATRIUM_AMD_RESET_POLLS; i++) {
		if (amd_mmio_read32(sc, regRESET_STATUS) != 0)
			break;
		DELAY(ATRIUM_AMD_RESET_DELAY);
	}
	if (i == ATRIUM_AMD_RESET_POLLS) {
		device_printf(dev, "GPU reset did not latch (RESET_STATUS "
		    "stuck at 0)\n");
		amd_teardown(sc);
		return (ENXIO);
	}
	amd_mmio_write32(sc, regRESET_ACK, 1);
	if (amd_mmio_read32(sc, regRESET_STATUS) != 0) {
		device_printf(dev, "GPU reset window did not close after ACK "
		    "(RESET_STATUS still 1)\n");
		amd_teardown(sc);
		return (ENXIO);
	}
	device_printf(dev, "GPU reset complete (REQ -> STATUS=1 -> ACK -> "
	    "STATUS=0)\n");

	/*
	 * Step 2 — GMC init: allocate the GPUVM page-directory base for the
	 * kernel context (VMID 0), program it, and enable paging. After this
	 * the device DMA-walks the page tables amd_gpuvm_map builds.
	 */
	sc->pdb_kva = amd_dma_alloc(sc, &sc->pdb_gpa);
	if (sc->pdb_kva == NULL) {
		device_printf(dev, "failed to allocate page-directory base\n");
		amd_teardown(sc);
		return (ENOMEM);
	}
	amd_mmio_write32(sc, regPT_BASE_LO, (uint32_t)(sc->pdb_gpa & 0xffffffff));
	amd_mmio_write32(sc, regPT_BASE_HI, (uint32_t)(sc->pdb_gpa >> 32));
	amd_mmio_write32(sc, regGMC_ENABLE, 1);

	/*
	 * Step 3 — IH init: stand up the interrupt-handler ring so the CP's
	 * end-of-pipe RELEASE_MEM has somewhere to write its cookie. (We don't
	 * install an ISR at this milestone — the fence read-back is the proof;
	 * the IRQ path is exercised but its delivery is not yet consumed.)
	 */
	if (amd_dma_alloc(sc, &ih_gpa) == NULL) {
		device_printf(dev, "failed to allocate IH ring\n");
		amd_teardown(sc);
		return (ENOMEM);
	}
	amd_mmio_write32(sc, regIH_BASE_LO, (uint32_t)(ih_gpa & 0xffffffff));
	amd_mmio_write32(sc, regIH_BASE_HI, (uint32_t)(ih_gpa >> 32));
	amd_mmio_write32(sc, regIH_SIZE, 256);

	/*
	 * Step 4 — load CP firmware (models the PSP loading the CP microcode).
	 * Stage the ucode version, then activate. The model refuses a version
	 * below CP_FW_MIN_VERSION; we stage exactly the minimum since there is
	 * no real blob behind the model. cp_fw_loaded is not read-back-able,
	 * so this is verified by construction (correct version, post-reset
	 * order) — its effect shows up in step 6 when the doorbell is honored.
	 */
	amd_mmio_write32(sc, regFW_CP_VERSION, ATRIUM_AMD_CP_FW_VERSION);
	amd_mmio_write32(sc, regFW_CP_LOAD, 1);
	device_printf(dev, "CP firmware loaded (version 0x%x)\n",
	    ATRIUM_AMD_CP_FW_VERSION);

	/*
	 * Step 5 — initialize the MES scheduler. Gated on the CP firmware
	 * above; the MES is what reads a queue descriptor and activates a
	 * queue, so it must be up before the submit in step 6.
	 */
	amd_mmio_write32(sc, regMES_INIT, 1);
	device_printf(dev, "MES initialized — GPU alive\n");

	/*
	 * Step 6 (milestone 3) — proof of life: map a ring + fence, lay a PM4
	 * [NOP, RELEASE_MEM] ring, map queue 0, ring the doorbell, read back
	 * the fence the CP DMA-wrote. A correct magic value end-to-end proves
	 * GPUVM translation + queue map + PM4 execution + DMA write-back all
	 * work — the first positive confirmation, not a by-construction one.
	 */
	fence = amd_submit_runjob(sc);
	if (fence == ATRIUM_AMD_FENCE_MAGIC)
		device_printf(dev, "submit OK: fence = 0x%016jx (ring drained, "
		    "CP wrote it back)\n", (uintmax_t)fence);
	else
		device_printf(dev, "submit FAILED: fence = 0x%016jx, expected "
		    "0x%016jx\n", (uintmax_t)fence,
		    (uintmax_t)ATRIUM_AMD_FENCE_MAGIC);

	/*
	 * Step 7 (milestone 4) — real compute: dispatch an INC kernel on a
	 * second queue (MEC HQD path) and read back results that depend on the
	 * input. Where step 6 proved a ring drains, this proves the GPU
	 * computes.
	 */
	amd_dispatch_compute(sc);

	return (0);
}

static int
atrium_amd_detach(device_t dev)
{
	struct atrium_amd_softc *sc = device_get_softc(dev);

	amd_teardown(sc);
	return (0);
}

static device_method_t atrium_amd_methods[] = {
	DEVMETHOD(device_probe,		atrium_amd_probe),
	DEVMETHOD(device_attach,	atrium_amd_attach),
	DEVMETHOD(device_detach,	atrium_amd_detach),
	DEVMETHOD_END
};

static driver_t atrium_amd_driver = {
	"atrium_gpu_amd",
	atrium_amd_methods,
	sizeof(struct atrium_amd_softc),
};

DRIVER_MODULE(atrium_gpu_amd, pci, atrium_amd_driver, NULL, NULL);
MODULE_DEPEND(atrium_gpu_amd, pci, 1, 1, 1);
MODULE_VERSION(atrium_gpu_amd, 4);
