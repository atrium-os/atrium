/*
 * atrium-gpu-amd — internal driver header (shared across the per-block .c
 * files; design §4.2). Holds the softc, the GFX12/SIM register map, the leaf
 * MMIO/PM4 mechanics (inline — §3.3), and the cross-file prototypes.
 *
 * Layout of the kmod (one module, several files; the §4.1 three-kmod split is
 * deferred until display lands):
 *   module.c    newbus probe/attach/detach + /dev/atrium-gpu0 cdev + teardown
 *   firmware.c  PSP/MES bring-up: reset, CP firmware load, MES init
 *   gmc.c       Graphics Memory Controller: GPUVM page tables, GMC/IH init
 *   bo.c        DMA-page allocator + buffer-object handle table
 *   cp.c        Command Processor: queue map + doorbell submit, compute state
 *   ioctl.c     the ATRIUM_GPU_IOC_* switch (the userspace ABI handlers)
 */
#ifndef _ATRIUM_GPU_AMD_H_
#define _ATRIUM_GPU_AMD_H_

#include <sys/param.h>
#include <sys/systm.h>
#include <sys/bus.h>
#include <sys/conf.h>
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

/* BAR5 = 256 KiB MMIO register file; BAR2 = the doorbell page (dev-ref §2,§5). */
#define ATRIUM_AMD_REGS_BAR	PCIR_BAR(5)
#define ATRIUM_AMD_DOORBELL_BAR	PCIR_BAR(2)

/*
 * Register offsets within BAR5. A real GFX12 register's absolute BAR5 offset
 * = APER_<block> + reg_dword*4 (device-reference §3): APER_GC=0x0_0000,
 * APER_OSS=0x1_0000, APER_SIM=0x2_0000. The SIM aperture has no single real-HW
 * analog — it models the PSP/MES/reset handshake + the stubbed compute state.
 * Offsets are spelled out absolutely so a reader can match them to the model
 * (engine/src/device.rs `regs`) without arithmetic.
 */
#define regSIM_ID		0x00	/* identity probe: reads GPUSIM_MAGIC */
#define SIM_ID_MAGIC		0x47505553u /* 'G','P','U','S' */
#define regGRBM_STATUS		0x3690	/* GC: gpusim models as 0 (functional) */

#define regFW_CP_VERSION	0x20050	/* SIM: staged CP ucode version (w) */
#define regFW_CP_LOAD		0x20054	/* SIM: w 1 = validate + activate CP */
#define regRESET_REQ		0x20058	/* SIM: w 1 = begin full GPU reset */
#define regRESET_STATUS		0x2005c	/* SIM: r 1 = reset latched, awaiting ack */
#define regRESET_ACK		0x20060	/* SIM: w 1 = ack / close reset window */
#define regMES_INIT		0x20068	/* SIM: w 1 = init MES (needs CP fw) */

#define regGMC_ENABLE		0x5890	/* GC: GCVM_CONTEXT0_CNTL (enable paging) */
#define regPT_BASE_LO		0x5a3c	/* GC: GCVM_CONTEXT0_PAGE_TABLE_BASE_LO32 */
#define regPT_BASE_HI		0x5a40	/* GC: ..._HI32 */
#define regTLB_INVALIDATE	0x591c	/* GC: GCVM_INVALIDATE_ENG0_REQ */
#define regIH_SIZE		0x10200	/* OSS: IH_RB_CNTL (ring size in entries) */
#define regIH_WPTR		0x10208	/* OSS: IH_RB_WPTR (device's write pointer) */
#define regIH_BASE_LO		0x1020c	/* OSS: IH_RB_BASE */
#define regIH_BASE_HI		0x10210	/* OSS: IH_RB_BASE_HI */

/* Interrupt-handler ring: each entry is a 16-byte cookie [cause:u32, ring:u32]. */
#define ATRIUM_AMD_IH_ENTRIES	256
#define ATRIUM_AMD_IH_COOKIE	16

#define regCP_RB0_BASE		0x7780	/* GC: gfx ring base (holds base>>8) */
#define regCP_RB0_CNTL		0x7784	/* GC: gfx ring size (bytes) */
#define regCP_RB_DOORBELL_CONTROL 0x7a34 /* GC: gfx ring doorbell offset (BAR2) */
#define regCP_ME_CNTL		0x200c	/* GC: w 0 = clear ME halt -> run */

#define regHQD_SELECT		0x20080	/* SIM: which HQD the CP_HQD regs program */
#define regCP_HQD_PQ_BASE	0x7ec4	/* GC: HQD ring base (holds base>>8) */
#define regCP_HQD_PQ_CONTROL	0x7ee8	/* GC: HQD ring size */
#define regCP_HQD_PQ_DOORBELL_CONTROL 0x7ee0 /* GC: HQD doorbell offset (BAR2) */
#define regCP_HQD_ACTIVE	0x7eac	/* GC: w 1 = activate HQD (gated on MES) */
#define regCOMPUTE_KERNEL	0x20200	/* SIM: built-in kernel selector */
#define regCOMPUTE_SRC_LO	0x20204	/* SIM: source buffer GPU-VA (lo/hi) */
#define regCOMPUTE_SRC_HI	0x20208
#define regCOMPUTE_DST_LO	0x2020c	/* SIM: dest buffer GPU-VA (lo/hi) */
#define regCOMPUTE_DST_HI	0x20210

/*
 * Graphics DRAW state (SIM aperture; the stubbed shader register set — the
 * SoftwareBackend rasterizer stands in). Vertices are 24 bytes (NDC x,y,z +
 * texcoord u,v as f32, then an RGBA8 color); the RT is RGBA8 width×height.
 * DEPTH/TEX = 0 disable depth test / texturing (use the interpolated color).
 */
#define regDRAW_VTX_LO		0x20214	/* SIM: vertex buffer GPU-VA (lo/hi) */
#define regDRAW_VTX_HI		0x20218
#define regDRAW_RT_LO		0x2021c	/* SIM: render-target GPU-VA (lo/hi) */
#define regDRAW_RT_HI		0x20220
#define regDRAW_RT_DIM		0x20224	/* SIM: width<<16 | height */
#define regDEPTH_LO		0x20228	/* SIM: depth buffer GPU-VA (0 = no test) */
#define regDEPTH_HI		0x2022c
#define regTEX_LO		0x20230	/* SIM: texture GPU-VA (0 = vertex color) */
#define regTEX_HI		0x20234
#define regBLEND_ENABLE		0x2023c	/* SIM: 1 = alpha blend (src-over) */

/* CP firmware: minimum ucode version the model accepts (CP_FW_MIN_VERSION). */
#define ATRIUM_AMD_CP_FW_VERSION 0x40
/* Reset poll: model latches synchronously; poll-with-timeout is the HW shape. */
#define ATRIUM_AMD_RESET_POLLS	1000	/* ×10us = 10ms budget */
#define ATRIUM_AMD_RESET_DELAY	10	/* us between polls */

/*
 * GPUVM 2-level page table (residency.rs): PDE at pdb[va>>21], PTE at
 * pt[va>>12], each 8 bytes = (page-aligned phys) | PTE_VALID(bit 0).
 */
#define ATRIUM_AMD_PTE_VALID	0x1ULL
#define ATRIUM_AMD_PD_SHIFT	21
#define ATRIUM_AMD_PT_SHIFT	12
#define ATRIUM_AMD_PT_MASK	0x1ff

/* PM4 type-3 header layout (public PM4; opcodes verified vs kfd_pm4_opcodes.h). */
#define PM4_TYPE3		3u
#define IT_NOP			0x10u
#define IT_RELEASE_MEM		0x49u
#define IT_DISPATCH_DIRECT	0x15u
#define IT_DRAW_INDEX_AUTO	0x2du

/* Per-engine fixed queue/doorbell assignment (no queue manager yet). */
#define ATRIUM_AMD_RING_BYTES	256	/* CP ring-size register value */
#define ATRIUM_AMD_GFX_QID	0
#define ATRIUM_AMD_GFX_DOORBELL	0x0
#define ATRIUM_AMD_COMPUTE_QID	1
#define ATRIUM_AMD_COMPUTE_DOORBELL 0x8

/* GPU-VA bump allocator: BOs are placed at BO_VA_BASE, BO_VA_BASE+page, ... */
#define ATRIUM_AMD_BO_VA_BASE	0x10000000ULL

/* Fixed registries (page tables + IH + BOs share dma[]; bo[] adds VA/handle). */
#define ATRIUM_AMD_MAX_DMA	64
#define ATRIUM_AMD_MAX_BO	48

/*
 * A page of DMA-able guest memory: kernel VA (CPU side) + guest-physical
 * address (what the device DMA-walks). In a VM with no IOMMU on this device,
 * gpa == vtophys(kva).
 */
struct atrium_amd_dma_page {
	void		*kva;
	vm_paddr_t	 gpa;
};

/* A buffer object: a DMA page exposed to userspace at a GPU virtual address. */
struct atrium_amd_bo {
	void		*kva;
	uint64_t	 gpu_va;
	uint64_t	 size;
};

struct atrium_amd_softc {
	device_t	 dev;
	struct resource	*regs;		/* BAR5 MMIO register file */
	int		 regs_rid;
	struct resource	*doorbell;	/* BAR2 doorbell page */
	int		 doorbell_rid;
	struct cdev	*cdev;		/* /dev/atrium-gpu0 */

	struct resource	*msix_table;	/* BAR holding the MSI-X table (BAR4) */
	int		 msix_table_rid;
	struct resource	*irq;		/* MSI-X vector 0 */
	int		 irq_rid;
	void		*intr_cookie;
	int		 msix_enabled;	/* 1 = interrupt mode, 0 = poll mode */
	void		*ih_kva;	/* interrupt-handler ring (CPU side) */
	uint32_t	 ih_rptr;	/* our read pointer into the IH ring */
	volatile u_int	 irq_count;	/* interrupts serviced (ISR vs reader) */

	void		*pdb_kva;	/* GPUVM page-directory base (VMID 0) */
	vm_paddr_t	 pdb_gpa;
	uint64_t	 next_gpu_va;	/* bump allocator for BO virtual addresses */

	struct atrium_amd_dma_page dma[ATRIUM_AMD_MAX_DMA];
	int		 n_dma;
	struct atrium_amd_bo bo[ATRIUM_AMD_MAX_BO];
	int		 n_bo;
};

/*
 * Leaf MMIO/PM4 mechanics (§3.3 / §7.1 — pure, no driver state beyond the BAR
 * handle, no control flow). Inline in the header: they are one-liners used
 * across every block, so a ring_helpers.c translation unit would be all
 * boilerplate and no substance. The single place raw bus_space access lives.
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

/* type[31:30] | (body_dwords-1)[29:16] | opcode[15:8]. */
static inline uint32_t
amd_pm4_type3_header(uint32_t opcode, uint32_t body_dwords)
{
	return ((PM4_TYPE3 << 30) | (((body_dwords - 1) & 0x3fff) << 16) |
	    (opcode << 8));
}

/* bo.c — DMA pages + buffer objects */
void	*amd_dma_alloc(struct atrium_amd_softc *sc, vm_paddr_t *gpa_out);
void	*amd_dma_kva(struct atrium_amd_softc *sc, vm_paddr_t gpa);
int	 amd_bo_alloc(struct atrium_amd_softc *sc, uint64_t size,
	    uint32_t *handle_out, uint64_t *gpu_va_out);
struct atrium_amd_bo *amd_bo_lookup(struct atrium_amd_softc *sc,
	    uint32_t handle);

/* gmc.c — GPUVM */
int	 amd_gpuvm_map(struct atrium_amd_softc *sc, uint64_t va,
	    vm_paddr_t phys);
int	 amd_gmc_init(struct atrium_amd_softc *sc);

/* firmware.c — bring-up */
int	 amd_reset(struct atrium_amd_softc *sc);
void	 amd_firmware_load(struct atrium_amd_softc *sc);
void	 amd_mes_init(struct atrium_amd_softc *sc);

/* cp.c — submission */
int	 amd_submit(struct atrium_amd_softc *sc, struct atrium_amd_bo *ring,
	    uint32_t n_dwords, uint32_t engine);
void	 amd_set_compute(struct atrium_amd_softc *sc, uint32_t kernel,
	    uint64_t src_va, uint64_t dst_va);
void	 amd_set_draw(struct atrium_amd_softc *sc, uint64_t vtx_va,
	    uint64_t rt_va, uint32_t width, uint32_t height);

/* irq.c — MSI-X interrupt setup + the ISR that drains the IH ring */
int	 amd_irq_setup(struct atrium_amd_softc *sc);
void	 amd_irq_teardown(struct atrium_amd_softc *sc);

/* ioctl.c — the cdev character-device switch */
extern struct cdevsw atrium_amd_cdevsw;

#endif /* _ATRIUM_GPU_AMD_H_ */
