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
#include <sys/capsicum.h>
#include <sys/conf.h>
#include <sys/event.h>
#include <sys/fcntl.h>
#include <sys/file.h>
#include <sys/filedesc.h>
#include <sys/kernel.h>
#include <sys/lock.h>
#include <sys/malloc.h>
#include <sys/mutex.h>
#include <sys/proc.h>
#include <sys/rman.h>
#include <sys/selinfo.h>
#include <sys/stat.h>
#include <sys/user.h>

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
#define regGPU_RESET		0x36a0	/* GC: GRBM_SOFT_RESET — per-block GFX soft
					 * reset (gpu-scoped: recovers a hung engine,
					 * leaves VRAM + the display block intact).
					 * Distinct from the device-wide FLR below. */

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
#define regCP_SUBMIT_DEADLINE_NS 0x20084 /* SIM: frame deadline (ns) the next doorbell stamps on its queue */
#define regCP_HQD_PQ_BASE	0x7ec4	/* GC: HQD ring base (holds base>>8) */
#define regCP_HQD_PQ_CONTROL	0x7ee8	/* GC: HQD ring size */
#define regCP_HQD_PQ_DOORBELL_CONTROL 0x7ee0 /* GC: HQD doorbell offset (BAR2) */
#define regCP_HQD_ACTIVE	0x7eac	/* GC: w 1 = activate HQD (gated on MES) */
/*
 * The COMPUTE state (SIM 0x200..0x210) and DRAW state (SIM 0x214..0x240)
 * registers are deliberately NOT named here: the kernel never writes them.
 * Userspace carries that state in the submitted ring as SET_SH_REG packets the
 * CP applies (the opaque-blob submit of ABI-v2); the register layout is the
 * userspace<->firmware contract, not the kernel's concern.
 */

/*
 * Per-context GPUVM (SIM aperture). A user address space (VMID 1..15) gets its
 * own page-directory base: select the VMID, then program its base. The
 * per-queue QF_VMID field (abstract Q_BASE block, q(qid)=Q_BASE+qid*STRIDE)
 * tells the CP which context a queue's submissions translate under — this is
 * what makes the same GPU-VA resolve to different memory per process.
 */
#define regVM_CTX_SELECT	0x2006c	/* SIM: which VMID the next regs program */
#define regVM_CTX_PT_BASE_LO	0x20070	/* SIM: selected VMID's page-dir base (lo/hi) */
#define regVM_CTX_PT_BASE_HI	0x20074
#define ATRIUM_AMD_Q_BASE	0x20100	/* SIM: per-queue config block base */
#define ATRIUM_AMD_Q_STRIDE	0x20	/* bytes per queue */
#define ATRIUM_AMD_QF_VMID	0x14	/* offset of the VMID field within a queue */

/*
 * Display block (APER_DISP = 0x3_0000; model: engine/src/display.rs `regs`).
 * Architecturally independent of GFX/compute — its own register aperture.
 */
#define regDISP_CONNECTOR_STATUS 0x30000 /* r: bit0 = connected (HPD) */
#define regDISP_DDC_OFFSET	0x30004	/* w: EDID byte offset to read next */
#define regDISP_DDC_DATA	0x30008	/* r: EDID byte at DDC_OFFSET (0xff = no DDC) */
#define regDISP_FB_BASE_LO	0x3000c	/* w: scanout FB VRAM offset (lo/hi) */
#define regDISP_FB_BASE_HI	0x30010
#define regDISP_FB_SIZE		0x30014	/* w: FB size in bytes */
#define regDISP_SET_MODE	0x30018	/* w 1: program the connector's EDID mode */
#define regDISP_FLIP		0x3001c	/* w: bit0 = vsync; the write triggers a flip */
#define regDISP_CONNECTOR_TYPE	0x30034	/* r: §8 interface type code */
#define regDISP_CFG_CONNECTOR_TYPE 0x30038 /* w: set connector type (test/bring-up) */
#define regDISP_CFG_PLUG_MODE	0x3003c	/* w: re-plug advertising a built-in mode */
#define regDISP_CFG_USBC	0x30040	/* w: USB-C alt-mode (0=USB, 2|4=enter N lanes) */
#define regDISP_USBC_LANES	0x30044	/* r: negotiated USB-C lane count */
#define regDISP_MST_ENABLE	0x30048	/* w1: (re)build a DP MST hub */
#define regDISP_MST_ADD_SINK	0x3004c	/* w: hot-plug an MST sink (mode code) */
#define regDISP_MST_SELECT	0x30050	/* w: select sink index for the MST queries */
#define regDISP_MST_SINK_COUNT	0x30054	/* r: number of MST sinks */
#define regDISP_MST_SINK_STARVED 0x30058 /* r: selected sink bandwidth-starved? */
#define regDISP_DPTRAIN_CABLE_RATE 0x3005c /* w: cable max rate (0=RBR..3=HBR3) */
#define regDISP_DPTRAIN_CABLE_LANES 0x30060 /* w: cable wired lanes */
#define regDISP_DPTRAIN_RUN	0x30064	/* w1: run link training */
#define regDISP_DPTRAIN_BW_MBPS	0x30068	/* r: trained bandwidth (MB/s) */
#define regDISP_DPTRAIN_TRAINED	0x3006c	/* r: 1 = a link trained */
#define regDISP_VBLANK_IRQ_EN	0x30070	/* w: 1 = raise an IH interrupt each vblank (DCN-like) */
#define regDISP_POWER_DEMAND_MW	0x30074	/* r: modeled display power demand, mW (energy federation) */
#define regDISP_POWER_BUDGET_MW	0x30078	/* w/r: granted power cap, mW (0 = uncapped) */
#define regDISP_POSTURE		0x3007c	/* w/r: power posture 0..10 (0 powersave..10 perf) */
#define regDISP_VBLANK_COUNT	0x30020	/* r: vblanks elapsed */
#define regDISP_DROPPED_FLIPS	0x30024	/* r: flips dropped by the depth-1 queue */
#define regDISP_FAULT		0x30028	/* r: last DisplayFault code (0 = none) */
#define regDISP_TEAR_LINE	0x3002c	/* r: first tear scanline (0xffffffff = none) */
#define ATRIUM_AMD_EDID_LEN	128

/*
 * Firmware energy-fair scheduler block (APER_SCHED = 0x4_0000; model:
 * engine/src/sched_regs.rs). The kernel programs weights, the device enforces.
 */
#define regSCHED_WEIGHT		0x40000	/* w: staged queue weight */
#define regSCHED_KERNEL_OPS	0x40004	/* w: staged per-dispatch ops */
#define regSCHED_KERNEL_BYTES	0x40008	/* w: staged per-dispatch bytes */
#define regSCHED_KERNEL_LEVEL	0x4000c	/* w: staged memory level (0..4) */
#define regSCHED_ADD_QUEUE	0x40010	/* w1: append a queue with the staged config */
#define regSCHED_RUN_ROUNDS	0x40014	/* w: run N energy-fair rounds */
#define regSCHED_SELECT		0x40018	/* w: select a queue for readback */
#define regSCHED_ENERGY_UJ	0x4001c	/* r: selected queue energy (uJ, telemetry) */
#define regSCHED_RUNS		0x40020	/* r: selected queue run count */
#define regSCHED_QUEUE_COUNT	0x40024	/* r: number of queues */
#define regSCHED_BUSY_US	0x40028	/* r: selected queue engine time (us, fairness) */
#define regSCHED_POWER_BUDGET_MW 0x4002c /* w: federation budget (0 = uncapped) */
#define regSCHED_POWER_DEMAND_MW 0x40030 /* r: average power demand (mW) */
#define regSCHED_ROUNDS_EXEC	0x40034	/* r: cumulative rounds executed */
#define regSCHED_DEADLINE	0x40038	/* w: selected queue's deadline, ns from now (0=clear) */
#define regSCHED_DEADLINE_WINDOW 0x4003c /* w: deadline window, ns (0=deadline-blind) */
#define regSCHED_NOW_NS		0x40040	/* r: scheduler virtual clock, ns */
#define regSCHED_POWER_POSTURE	0x40044	/* w/r: power posture 0..10 (0 powersave..10 perf) */

/*
 * Power-gating block (APER_PGATE = 0x5_0000; model: engine/src/pgate_regs.rs).
 * The driver reads which IP blocks are idle, power-gates them, and reads back the
 * power/energy. Exposes POWER gating (the driver-controlled lever); clock gating
 * is hardware-automatic, no register.
 */
#define regPGATE_NUM_BLOCKS	0x50000	/* r: number of gateable IP blocks */
#define regPGATE_BLOCK_BUSY	0x50004	/* r: bitmask of blocks with work in flight */
#define regPGATE_BLOCK_GATE	0x50008	/* rw: bitmask of blocks to power-gate */
#define regPGATE_SET_BUSY	0x5000c	/* w: set the busy bitmask (engine stand-in) */
#define regPGATE_POWER_MW	0x50010	/* r: current draw, milliwatts */
#define regPGATE_ENERGY_UJ	0x50014	/* r: accumulated energy, microjoules */
#define regPGATE_TICK_US	0x50018	/* w: advance N us, accruing energy */
#define regPGATE_SELECT		0x5001c	/* w: select a block for readbacks */
#define regPGATE_SEL_ACTIVE_MW	0x50020	/* r: selected block powered leakage (mW) */
#define regPGATE_SEL_GATED_MW	0x50024	/* r: selected block gated leakage (mW) */
#define regPGATE_SEL_EXIT_US	0x50028	/* r: selected block wake latency (us) */
#define regPGATE_NEXT_BUSY	0x5002c	/* w: foreknowledge - blocks the next job needs */
#define regPGATE_WAKE_STALL_US	0x50030	/* r: stall if NEXT_BUSY blocks aren't pre-woken */

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
#define ATRIUM_AMD_PTE_VRAM	0x2ULL	/* page is device VRAM (else System/GTT) */
#define ATRIUM_AMD_VRAM_BYTES	(256ULL << 20)	/* BAR0 VRAM capacity */
#define ATRIUM_AMD_PD_SHIFT	21
#define ATRIUM_AMD_PT_SHIFT	12
#define ATRIUM_AMD_PT_MASK	0x1ff

/* PM4 type-3 header layout (public PM4; opcodes verified vs kfd_pm4_opcodes.h). */
#define PM4_TYPE3		3u
#define IT_NOP			0x10u
#define IT_RELEASE_MEM		0x49u
#define IT_DISPATCH_DIRECT	0x15u
#define IT_DRAW_INDEX_AUTO	0x2du

/*
 * Per-engine fixed queue/doorbell assignment (no queue manager yet). Each
 * queue's doorbell sits at the start of its OWN BAR2 page so the page can be
 * mmap'd / SCM_RIGHTS-granted to one client without exposing other queues'
 * doorbells (real-HW per-queue doorbell granularity).
 */
#define ATRIUM_AMD_RING_BYTES	256	/* CP ring-size register value */
#define ATRIUM_AMD_GFX_QID	0
#define ATRIUM_AMD_GFX_DOORBELL	0x0000	/* doorbell page 0 */
#define ATRIUM_AMD_COMPUTE_QID	1
#define ATRIUM_AMD_COMPUTE_DOORBELL 0x1000 /* doorbell page 1 */

/*
 * Per-VM GPU-VA bump allocator: each address space places BOs at BO_VA_BASE,
 * BO_VA_BASE+page, ... across a contiguous run of NUM_PT page-directory
 * entries (NUM_PT * 512 pages, NUM_PT pre-allocated page-table pages). Two VMs
 * both start at BO_VA_BASE, so the same VA in different VMs resolves to
 * different memory — isolation. NUM_PT is sized so a full-screen offset-model
 * scanout (System staging BO + VRAM scanout BO, both framebuffer-sized) fits:
 * 16 PT pages = 8192 pages = 32 MiB of VA, room for a 1080p staging/scanout pair.
 */
#define ATRIUM_AMD_BO_VA_BASE	0x10000000ULL
#define ATRIUM_AMD_VM_NUM_PT	16	/* page-table pages per VM */
#define ATRIUM_AMD_VM_MAX_BO	(512 * ATRIUM_AMD_VM_NUM_PT) /* VA pages per VM */
#define ATRIUM_AMD_MAX_VMID	16	/* hardware contexts; 1..15 for user VMs */

/*
 * A page of DMA-able guest memory: kernel VA (CPU side) + guest-physical
 * address (what the device DMA-walks). In a VM with no IOMMU on this device,
 * gpa == vtophys(kva).
 */
struct atrium_amd_dma_page {
	void		*kva;
	vm_paddr_t	 gpa;		/* device-visible bus address */
	bus_dma_tag_t	 tag;
	bus_dmamap_t	 map;
};

/*
 * A buffer object: a DMA page mapped into GPUVM and exposed to userspace as a
 * file descriptor (fd-as-handle — ABI-v2 principle 2). Lifetime is the fd
 * refcount; the BO owns its own page (distinct from the internal dma[] pages
 * that back page tables / the IH ring). It carries a back-reference to the
 * device so fo_close can unmap + free without a handle table.
 */
struct atrium_amd_softc;
struct atrium_amd_vm;
struct atrium_amd_bo;
struct atrium_gpu_sched;	/* 'A' ABI structs (defined in atrium_gpu_amd_abi.h) */
struct atrium_gpu_powergate;

/*
 * Backend-supplied device capability values; the front-end assembles these into
 * the QUERY_CAPS TLV (the ABI version is the front-end's, the rest is backend).
 */
struct atrium_gpu_backend_caps {
	const char *vendor;	/* CAP_VENDOR */
	uint32_t    features;	/* CAP_FEATURES bitmap */
	uint64_t    va_base;	/* CAP_ADDRESS_SPACE */
	uint64_t    va_size;
	uint64_t    va_align;
	uint64_t    vram_bytes;	/* CAP_HEAPS device heap size */
};

/*
 * Backend ops — the per-transport hardware seam under the shared 'A' front-end
 * (docs/spec/atrium-gpu-driver-architecture.md §6). The cdev + ioctl dispatch,
 * the BO/VM/syncobj fd-objects, the bindings list, the syncobj timeline and the
 * caps TLV are transport-neutral and call ONLY through this vtable; a backend
 * (amd = GPUVM + PM4 over gpusim/real silicon; virtio = context + SUBMIT_3D)
 * supplies the hardware. The submit blob is opaque and the GPU-VA is backend-
 * assigned, which is what keeps the front-end neutral.
 */
struct atrium_gpu_backend_ops {
	const char *name;
	/* Stand up / tear down a VM's hardware address space (amd: VMID + GPUVM
	 * page tables + per-context PT base; virtio: a 3D context). The front-end
	 * owns the fd object + struct; the backend owns the hardware. vm_setup
	 * self-cleans on failure; vm_teardown undoes a successful setup. */
	int	(*vm_setup)(struct atrium_amd_softc *sc, struct atrium_amd_vm *vm);
	void	(*vm_teardown)(struct atrium_amd_vm *vm);
	/* Allocate / free a BO's backing store (amd: bus_dma System pages or a VRAM
	 * bump; virtio: a blob resource). Fills the BO's page list + size; the
	 * front-end owns the struct + fd + the cross-VM bindings. bo_alloc
	 * self-cleans on failure. */
	int	(*bo_alloc)(struct atrium_amd_softc *sc, struct atrium_amd_bo *bo,
		    uint64_t size, uint32_t flags);
	void	(*bo_free)(struct atrium_amd_bo *bo);
	/* Map / unmap one page of a BO into a VM at a GPU-VA (amd: a GPUVM PTE). */
	int	(*map_page)(struct atrium_amd_vm *vm, uint64_t va,
		    vm_paddr_t phys, int vram);
	void	(*unmap_page)(struct atrium_amd_vm *vm, uint64_t va);
	/* Submit a prepared ring on an engine in a VM (amd: PM4 ring + doorbell). */
	int	(*submit)(struct atrium_amd_softc *sc, struct atrium_amd_bo *ring,
		    uint32_t n_dwords, uint32_t engine, struct atrium_amd_vm *vm);
	/* Export a BO as a scanout handle (amd: absolute VRAM offset + size; only
	 * VRAM is scannable). EINVAL if the BO can't be scanned out. */
	int	(*export_scanout)(struct atrium_amd_bo *bo, uint64_t *vram_offset,
		    uint64_t *size);
	/* mmap a device aperture into userspace (amd: the per-queue doorbell page
	 * = a capability grant). Returns the physical addr + memattr for `offset`. */
	int	(*mmap)(struct atrium_amd_softc *sc, vm_ooffset_t offset,
		    vm_paddr_t *paddr, vm_memattr_t *memattr);
	/* Fill in the backend's device capability values (vendor, heaps, VA window). */
	void	(*get_caps)(struct atrium_amd_softc *sc,
		    struct atrium_gpu_backend_caps *caps);

	/*
	 * Optional ops — a backend that doesn't implement one leaves it NULL and
	 * the front-end returns ENOTSUP (capability-gated per v2). These are the
	 * advanced/vendor-specific surfaces beyond the common path.
	 */
	/* Map a queue's doorbell for user-mode submission (amd: program the MEC/CP
	 * queue at ring_va, return the doorbell page offset). */
	int	(*queue_program)(struct atrium_amd_softc *sc, uint64_t ring_va,
		    uint32_t engine, uint16_t vmid, uint32_t *doorbell_off);
	/* Recover a wedged engine (amd: reset + reload firmware + re-init MES). */
	int	(*gpu_reset)(struct atrium_amd_softc *sc);
	/* Energy-aware scheduler hint (amd: SCHED registers). */
	void	(*sched)(struct atrium_amd_softc *sc, struct atrium_gpu_sched *s);
	/* Power-gating policy (amd: GFXOFF / per-IP PG). */
	int	(*powergate)(struct atrium_amd_softc *sc,
		    struct atrium_gpu_powergate *p);
};

/*
 * A per-process GPU address space (ABI-v2 §5.2): its own VMID and 2-level page
 * table, programmed into the device's per-context page-directory registers. A
 * VM is a struct file; BOs created in it hold a reference, so it outlives them.
 * Page-table pages are pre-allocated (one PT page = 512 BOs in a 2 MiB span) so
 * mapping a BO never allocates under a lock.
 */
struct atrium_amd_vm {
	struct atrium_amd_softc *sc;
	struct file	*fp;		/* our own file (for KASSERT/debug) */
	uint16_t	 vmid;		/* 1..15 */
	struct atrium_amd_dma_page pdb;	/* page-directory page */
	struct atrium_amd_dma_page pt[ATRIUM_AMD_VM_NUM_PT]; /* page-table pages */
	uint64_t	 next_va;	/* bump allocator within this VM */
};

/*
 * A buffer object: a DMA page mapped into a VM's GPUVM and exposed to userspace
 * as a file descriptor (fd-as-handle — ABI-v2 principle 2). Lifetime is the fd
 * refcount; the BO owns its own pages and is independent of any VM (ABI-v2
 * principle 4), so the SAME object can be bound into several VMs at once — the
 * cross-address-space sharing path (a compositor importing a client's buffer).
 * Each binding holds a reference (vm_fp) on its VM, so every VM the BO is mapped
 * into outlives the BO and fo_close can unmap from all of them.
 */
#define ATRIUM_AMD_BO_MAX_PAGES	512	/* largest BO = 2 MiB; a 640x480x4 scanout
					 * FB is 300 pages, so a display FB must
					 * fit (small GPU BOs use a handful). */
#define ATRIUM_AMD_BO_MAX_BIND	8	/* VMs a BO can be shared into at once */

/* One mapping of a BO into a VM: its address space, a held ref on it, and the
 * base GPU-VA the BO occupies there (each VM assigns its own VA). */
struct atrium_amd_bo_binding {
	struct atrium_amd_vm *vm;
	struct file	*vm_fp;		/* held reference keeping that vm alive */
	uint64_t	 gpu_va;	/* base GPU-VA of the BO in that vm */
};

struct atrium_amd_bo {
	struct atrium_amd_softc *sc;
	struct atrium_amd_bo_binding bindings[ATRIUM_AMD_BO_MAX_BIND];
	int		 n_bindings;	/* VMs this BO is currently bound into */
	void		*kva;		/* CPU mapping (bus_dmamem, page-contiguous) */
	bus_dma_tag_t	 dmat;		/* per-BO DMA tag */
	bus_dmamap_t	 dmamap;	/* the BO's DMA mapping */
	int		 npages;	/* pages backing this BO */
	int		 vram;		/* 1 = VRAM-resident (pages[] are VRAM offsets,
					 * no kva/dmat); 0 = System/GTT (bus_dma) */
	bus_addr_t	 pages[ATRIUM_AMD_BO_MAX_PAGES]; /* per-page bus addrs / VRAM offsets */
	uint64_t	 size;
};

/*
 * A timeline syncobj: a monotonic 64-bit counter exposed as an fd
 * (ABI-v2 §5.6). A submission signals it on completion; userspace waits for a
 * value either blocking (SYNCOBJ_WAIT) or via kqueue — the fd is EVFILT_READ-
 * able, with the threshold passed in the kevent `data` field, so a compositor
 * folds GPU completion into one kevent() alongside input and timers. (v2's
 * separate per-threshold event_fd is deferred; the syncobj fd is the kqueue
 * source for now.) The struct-file refcount is its lifetime.
 */
struct atrium_amd_syncobj {
	struct atrium_amd_softc *sc;	/* for scrubbing the pending list on close */
	struct mtx	 lock;	/* guards value + the knote list */
	struct selinfo	 sel;	/* sel.si_note = the kqueue knlist */
	uint64_t	 value;
};

/*
 * A completion the ISR owes a syncobj. A submission that signals a syncobj
 * pushes one of these *before* ringing the doorbell; the ISR pops it on the
 * end-of-pipe interrupt and signals the syncobj — so a submission whose ring
 * parks on a cross-queue WAIT is signalled asynchronously, when a *later*
 * doorbell unblocks it, not inline. The syncobj's fo_close scrubs its entries
 * under sc->lock, which serializes against the ISR (no refcount / no ISR free).
 */
struct atrium_amd_pending {
	struct atrium_amd_syncobj *so;
	uint64_t	 value;
};
#define ATRIUM_AMD_MAX_PENDING	16
#define ATRIUM_AMD_IH_CAUSE_EOP	1	/* IH cookie cause: end-of-pipe (device.rs) */
#define ATRIUM_AMD_IH_CAUSE_VBLANK 2	/* IH cookie cause: DCN vertical blank */
#define ATRIUM_AMD_IH_NCAUSE	4	/* dispatch-table size (causes 0..N-1) */

/*
 * IH cause handler: the device-global interrupt ring is owned by the BASE
 * module, which drains it and demuxes by cookie cause (exactly as real silicon's
 * one IH ring carries GFX end-of-pipe AND DCN vblank to a single ISR). Each IP
 * module registers a handler for its own cause(s) — gpu for EOP, display for
 * VBLANK — so neither needs the other: the base routes the cause to whoever
 * registered. The handler is called with sc->lock HELD and `count` = how many of
 * that cause this interrupt drained (coalesced events report > 1).
 */
typedef void (*atrium_amd_ih_handler)(struct atrium_amd_softc *sc, int count);

/*
 * Device-reset coordination. A full FLR (mode-1 reset) is a DEVICE-global event:
 * on real silicon it resets every IP block — GFX *and* DCN (display) — so it
 * cannot be a thing one IP module does behind the others' backs. The base owns
 * the FLR (amd_flr) and the coordinator (amd_device_reset); each IP module
 * registers a prepare/restore pair so a device-lost recovery quiesces every
 * block before the FLR and re-initialises it after (amdgpu's pre_reset/post_reset
 * across IP blocks). prepare/restore run under sc->lock is NOT assumed — they may
 * sleep (firmware reload), so the coordinator calls them unlocked.
 *
 * Cold bring-up takes a different path: the base FLRs ONCE at device attach,
 * before any child exists, so no hooks are needed there — the children then
 * attach onto an already-clean device.
 */
enum {
	ATRIUM_AMD_IP_GPU = 0,		/* GFX/compute (gpu module) */
	ATRIUM_AMD_IP_DISPLAY,		/* DCN (display module) */
	ATRIUM_AMD_IP_COUNT
};
struct atrium_amd_reset_hooks {
	void (*prepare)(struct atrium_amd_softc *sc);	/* quiesce before FLR */
	void (*restore)(struct atrium_amd_softc *sc);	/* re-init after FLR */
};

struct atrium_amd_softc {
	device_t	 dev;
	const struct atrium_gpu_backend_ops *backend; /* hardware seam (gpu module) */
	struct resource	*regs;		/* BAR5 MMIO register file */
	int		 regs_rid;
	struct resource	*doorbell;	/* BAR2 doorbell page */
	int		 doorbell_rid;
	struct cdev	*cdev;		/* /dev/atrium-gpu0 (gpu module) */
	struct cdev	*display_cdev;	/* /dev/atrium-display0 (display module) */
	int		energy_member;	/* gpu energy-federation id, -1 = none */
	int		display_energy_member; /* display energy-federation id, -1 = none */

	/*
	 * Vblank knote list: EVFILT_READ knotes registered on /dev/atrium-display0
	 * (display module) hang here, but the GPU module's IH ISR is what fires them
	 * (it sees the vblank interrupt). It lives in the SHARED softc, inited by the
	 * base (pci) module under sc->lock, so the ISR's KNOTE_LOCKED is valid whether
	 * or not the display module is loaded (empty list = harmless no-op walk).
	 */
	struct selinfo	 display_sel;

	struct resource	*msix_table;	/* BAR holding the MSI-X table (BAR4) */
	int		 msix_table_rid;
	struct resource	*irq;		/* MSI-X vector 0 */
	int		 irq_rid;
	void		*intr_cookie;
	int		 msix_enabled;	/* 1 = interrupt mode, 0 = poll mode */

	/*
	 * Device-reset coordination (owned by the base): each IP module registers a
	 * prepare/restore pair so a device-wide FLR quiesces + re-inits every block.
	 */
	struct atrium_amd_reset_hooks reset_hooks[ATRIUM_AMD_IP_COUNT];
	int		 display_vblank_armed;	/* display module: mode set + vblank IRQ on */

	/*
	 * Device-global IH (interrupt-handler) ring + dispatch, owned by the BASE
	 * module. The ring is one coherent DMA page the device write-walks; the base
	 * ISR drains it and routes each cookie by cause to ih_handler[cause] (set by
	 * the gpu/display modules under sc->lock). ih_kva/tag/map back the page.
	 */
	void		*ih_kva;	/* interrupt-handler ring (CPU side) */
	bus_dma_tag_t	 ih_tag;	/* bus_dma tag for the ring page */
	bus_dmamap_t	 ih_map;	/* bus_dma map for the ring page */
	uint32_t	 ih_rptr;	/* our read pointer into the IH ring */
	atrium_amd_ih_handler ih_handler[ATRIUM_AMD_IH_NCAUSE]; /* cause -> handler */
	u_int		 irq_count;	/* interrupts serviced (atomic vs reader) */
	struct mtx	 lock;		/* guards fence-wait sleep/wakeup + pending */
	int		 lock_inited;
	struct atrium_amd_pending pending[ATRIUM_AMD_MAX_PENDING];
	int		 n_pending;	/* completions the ISR owes (FIFO) */

	uint32_t	 vmid_bitmap;	/* allocated VMIDs (bit N = VMID N in use) */
	int		 vm_count;	/* live vm_fds */

	int		 bo_count;	/* live bo_fds; detach refuses if > 0 */
	uint64_t	 vram_next;	/* VRAM bump allocator cursor (base-managed) */
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

/* bo.c — DMA pages (the GPUVM page tables) + fd-backed buffer objects */
int	 amd_dma_page_alloc(struct atrium_amd_softc *sc,
	    struct atrium_amd_dma_page *p);
void	 amd_dma_page_free(struct atrium_amd_dma_page *p);

/*
 * IH ring + dispatch — owned by the BASE (pci) module (ih.c). amd_ih_init stands
 * up the device-global interrupt ring; amd_irq_setup hooks the ISR that drains
 * it and routes by cause. The gpu/display modules don't call these — they only
 * register a handler for their cause via amd_ih_set_handler (below).
 */
int	 amd_ih_init(struct atrium_amd_softc *sc);
void	 amd_ih_fini(struct atrium_amd_softc *sc);

/*
 * Register/clear the handler for an IH cause. Stored in the shared softc under
 * sc->lock (the ISR reads + calls it under that same lock), so a module sets its
 * handler on attach and clears it (NULL) on detach with no races against a
 * firing interrupt. Inline in the header → no cross-module symbol, each module
 * compiles its own copy (the function it points at is its own).
 */
static inline void
amd_ih_set_handler(struct atrium_amd_softc *sc, int cause,
    atrium_amd_ih_handler fn)
{
	mtx_lock(&sc->lock);
	if (cause >= 0 && cause < ATRIUM_AMD_IH_NCAUSE)
		sc->ih_handler[cause] = fn;
	mtx_unlock(&sc->lock);
}
int	 amd_bo_create_fd(struct atrium_amd_softc *sc, struct thread *td,
	    uint64_t size, uint32_t flags, int *out_fd);
int	 amd_bo_fget(struct thread *td, int fd, struct file **out_fp,
	    struct atrium_amd_bo **out_bo);
/* Bind a BO into `vm` at *va (0 = auto), adding a binding (a BO may be bound
 * into several VMs — sharing). On success the binding takes over vm_fp; on error
 * the caller keeps it. */
int	 amd_bo_bind(struct atrium_amd_bo *bo, struct atrium_amd_vm *vm,
	    struct file *vm_fp, uint64_t *va);
/* The BO's base GPU-VA in `vm`, or 0 if it is not bound there. */
uint64_t amd_bo_gpu_va(struct atrium_amd_bo *bo, struct atrium_amd_vm *vm);
/* amd backend: allocate/free a BO's backing (bus_dma System pages or a VRAM
 * bump); the front-end owns the struct/fd/bindings. */
int	 amd_bo_backing_alloc(struct atrium_amd_softc *sc,
	    struct atrium_amd_bo *bo, uint64_t size, uint32_t flags);
void	 amd_bo_backing_free(struct atrium_amd_bo *bo);
/* amd backend: export a VRAM BO as a {vram_offset, size} scanout handle. */
int	 amd_export_scanout(struct atrium_amd_bo *bo, uint64_t *vram_offset,
	    uint64_t *size);
/* amd backend: mmap the per-queue doorbell page (the capability grant). */
int	 amd_doorbell_mmap(struct atrium_amd_softc *sc, vm_ooffset_t offset,
	    vm_paddr_t *paddr, vm_memattr_t *memattr);
extern const struct fileops atrium_amd_bo_fileops;

/* vm.c — per-process GPU address spaces (fd-backed) + GPUVM page tables */
int	 amd_vm_create_fd(struct atrium_amd_softc *sc, struct thread *td,
	    int *out_fd);
int	 amd_vm_fget(struct thread *td, int fd, struct file **out_fp,
	    struct atrium_amd_vm **out_vm);
int	 amd_vm_map(struct atrium_amd_vm *vm, uint64_t va, vm_paddr_t phys,
	    int vram);
void	 amd_vm_unmap(struct atrium_amd_vm *vm, uint64_t va);
/* amd backend hardware setup/teardown of a VM's GPUVM (the front-end owns the
 * struct + fd; these own the VMID + page tables + PT base). */
int	 amd_vm_setup(struct atrium_amd_softc *sc, struct atrium_amd_vm *vm);
void	 amd_vm_teardown(struct atrium_amd_vm *vm);
extern const struct fileops atrium_amd_vm_fileops;

/* gmc.c — GMC bring-up (GPUVM paging enable) */
int	 amd_gmc_init(struct atrium_amd_softc *sc);

/* firmware.c (GPU module) — GFX-block bring-up + the gpu-scoped soft reset */
void	 amd_grbm_soft_reset(struct atrium_amd_softc *sc);
void	 amd_firmware_load(struct atrium_amd_softc *sc);
void	 amd_mes_init(struct atrium_amd_softc *sc);

/*
 * reset.c (BASE module) — the device-wide FLR + its coordinator. amd_flr drives
 * the raw REQ/poll/ACK handshake (used directly for the cold reset at device
 * attach). amd_device_reset is the recovery path: it runs every registered IP's
 * prepare hook, FLRs, then runs every restore hook — so a device-lost reset
 * doesn't corrupt a block (e.g. the display) that didn't ask for it.
 */
int	 amd_flr(struct atrium_amd_softc *sc);
int	 amd_device_reset(struct atrium_amd_softc *sc);

/*
 * vram.c (BASE module) — the device VRAM bump allocator. VRAM is a *device*
 * resource (any IP block may carve from it — the gpu's BOs today, a display
 * cursor/overlay plane tomorrow), so the base owns the cursor. Bumps under
 * sc->lock; *out_off is a byte offset into the VRAM aperture. ENOMEM when full.
 */
int	 amd_vram_alloc(struct atrium_amd_softc *sc, uint64_t size, uint64_t *out_off);

/*
 * Register an IP module's reset prepare/restore hooks (NULL to clear on detach).
 * Stored in the shared softc under sc->lock; the coordinator snapshots them, so
 * a module clears its hooks on detach with no race against a concurrent reset.
 * Inline → no cross-module symbol (the hooks point at the registering module).
 */
static inline void
amd_reset_register(struct atrium_amd_softc *sc, int ip,
    void (*prepare)(struct atrium_amd_softc *),
    void (*restore)(struct atrium_amd_softc *))
{
	mtx_lock(&sc->lock);
	if (ip >= 0 && ip < ATRIUM_AMD_IP_COUNT) {
		sc->reset_hooks[ip].prepare = prepare;
		sc->reset_hooks[ip].restore = restore;
	}
	mtx_unlock(&sc->lock);
}

/* cp.c — submission (under a VM's VMID) */
int	 amd_submit(struct atrium_amd_softc *sc, struct atrium_amd_bo *ring,
	    uint32_t n_dwords, uint32_t engine, struct atrium_amd_vm *vm);
int	 amd_queue_program(struct atrium_amd_softc *sc, uint64_t ring_va,
	    uint32_t engine, uint16_t vmid, uint32_t *doorbell_off);

/* cp.c — firmware energy-fair scheduler (the kernel programs weights) */
struct atrium_gpu_sched;
void	 amd_sched(struct atrium_amd_softc *sc, struct atrium_gpu_sched *s);

/* pgate.c — power-gate idle IP blocks (the driver-controlled lever) */
struct atrium_gpu_powergate;
int	 amd_powergate(struct atrium_amd_softc *sc, struct atrium_gpu_powergate *p);

/*
 * The display block (APER_DISP register helpers + the /dev/atrium-display0
 * cdev) lives in a SEPARATE module, atrium_gpu_amd_display.ko (display/), which
 * shares this softc + the regDISP_* defs + the mmio accessors above but has its
 * own newbus child and ABI (atrium_display_abi.h). The GPU module does not
 * reference it.
 */

/*
 * mmap offset (passed to mmap() on the device fd) that maps the BAR2 doorbell
 * page into userspace for user-mode-queue submission. One page; the queue's
 * doorbell lives at its byte offset within it.
 */
#define ATRIUM_AMD_DOORBELL_MMAP_OFF	0

/* sync.c — timeline syncobj fd (kqueue-able) */
int	 amd_syncobj_create_fd(struct atrium_amd_softc *sc, struct thread *td,
	    int *out_fd);
int	 amd_syncobj_fget(struct thread *td, int fd, struct file **out_fp,
	    struct atrium_amd_syncobj **out_so);
void	 amd_syncobj_signal(struct atrium_amd_syncobj *so, uint64_t value);
extern const struct fileops atrium_amd_syncobj_fileops;

/* ih.c (BASE module) — MSI-X interrupt setup + the device-global IH ring ISR */
int	 amd_irq_setup(struct atrium_amd_softc *sc);
void	 amd_irq_teardown(struct atrium_amd_softc *sc);

/*
 * irq.c (GPU module) — the EOP cause handler + its pending-completion list. The
 * base ISR routes IH_CAUSE_EOP here (registered via amd_ih_set_handler). Called
 * with sc->lock HELD: retires `count` end-of-pipe completions onto their
 * syncobjs and wakes fence waiters. The pending FIFO is pushed before a submit's
 * doorbell and scrubbed by a syncobj's fo_close.
 */
void	 amd_eop_handler(struct atrium_amd_softc *sc, int count);
void	 amd_pending_push(struct atrium_amd_softc *sc,
	    struct atrium_amd_syncobj *so, uint64_t value);
void	 amd_pending_scrub(struct atrium_amd_softc *sc,
	    struct atrium_amd_syncobj *so);

/* ioctl.c — the cdev character-device switch */
extern struct cdevsw atrium_amd_cdevsw;

#endif /* _ATRIUM_GPU_AMD_H_ */
