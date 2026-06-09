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

/*
 * Per-VM GPU-VA bump allocator: each address space places BOs at BO_VA_BASE,
 * BO_VA_BASE+page, ... within one page-directory entry's 2 MiB span (512
 * pages = one pre-allocated page-table page). Two VMs both start at BO_VA_BASE,
 * so the same VA in different VMs resolves to different memory — isolation.
 */
#define ATRIUM_AMD_BO_VA_BASE	0x10000000ULL
#define ATRIUM_AMD_VM_MAX_BO	512	/* BOs per VM (one PT page) */
#define ATRIUM_AMD_MAX_VMID	16	/* hardware contexts; 1..15 for user VMs */

/* Internal DMA-page registry (the IH ring; page tables are now per-VM). */
#define ATRIUM_AMD_MAX_DMA	8

/*
 * A page of DMA-able guest memory: kernel VA (CPU side) + guest-physical
 * address (what the device DMA-walks). In a VM with no IOMMU on this device,
 * gpa == vtophys(kva).
 */
struct atrium_amd_dma_page {
	void		*kva;
	vm_paddr_t	 gpa;
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
	void		*pdb_kva;	/* page-directory page */
	vm_paddr_t	 pdb_gpa;
	void		*pt_kva;	/* the single pre-allocated page-table page */
	vm_paddr_t	 pt_gpa;
	uint64_t	 next_va;	/* bump allocator within this VM */
};

/*
 * A buffer object: a DMA page mapped into a VM's GPUVM and exposed to userspace
 * as a file descriptor (fd-as-handle — ABI-v2 principle 2). Lifetime is the fd
 * refcount; the BO owns its own page and holds a reference (vm_fp) on the VM it
 * is mapped in, so fo_close can unmap from that VM.
 */
struct atrium_amd_bo {
	struct atrium_amd_softc *sc;
	struct atrium_amd_vm *vm;	/* the address space this BO is mapped in */
	struct file	*vm_fp;		/* held reference keeping vm alive */
	void		*kva;
	vm_paddr_t	 gpa;
	uint64_t	 gpu_va;
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
	u_int		 irq_count;	/* interrupts serviced (atomic vs reader) */
	struct mtx	 lock;		/* guards fence-wait sleep/wakeup + pending */
	int		 lock_inited;
	struct atrium_amd_pending pending[ATRIUM_AMD_MAX_PENDING];
	int		 n_pending;	/* completions the ISR owes (FIFO) */

	uint32_t	 vmid_bitmap;	/* allocated VMIDs (bit N = VMID N in use) */
	int		 vm_count;	/* live vm_fds */

	struct atrium_amd_dma_page dma[ATRIUM_AMD_MAX_DMA];
	int		 n_dma;
	int		 bo_count;	/* live bo_fds; detach refuses if > 0 */
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

/* bo.c — internal DMA pages + fd-backed buffer objects */
void	*amd_dma_alloc(struct atrium_amd_softc *sc, vm_paddr_t *gpa_out);
int	 amd_bo_create_fd(struct atrium_amd_softc *sc, struct thread *td,
	    uint64_t size, int *out_fd);
int	 amd_bo_fget(struct thread *td, int fd, struct file **out_fp,
	    struct atrium_amd_bo **out_bo);
/* Map an unbound BO into `vm` at *va (0 = auto). On success the BO takes over
 * vm_fp; on error the caller keeps it. */
int	 amd_bo_bind(struct atrium_amd_bo *bo, struct atrium_amd_vm *vm,
	    struct file *vm_fp, uint64_t *va);
extern const struct fileops atrium_amd_bo_fileops;

/* vm.c — per-process GPU address spaces (fd-backed) + GPUVM page tables */
int	 amd_vm_create_fd(struct atrium_amd_softc *sc, struct thread *td,
	    int *out_fd);
int	 amd_vm_fget(struct thread *td, int fd, struct file **out_fp,
	    struct atrium_amd_vm **out_vm);
int	 amd_vm_map(struct atrium_amd_vm *vm, uint64_t va, vm_paddr_t phys);
void	 amd_vm_unmap(struct atrium_amd_vm *vm, uint64_t va);
extern const struct fileops atrium_amd_vm_fileops;

/* gmc.c — GMC/IH bring-up */
int	 amd_gmc_init(struct atrium_amd_softc *sc);

/* firmware.c — bring-up */
int	 amd_reset(struct atrium_amd_softc *sc);
void	 amd_firmware_load(struct atrium_amd_softc *sc);
void	 amd_mes_init(struct atrium_amd_softc *sc);

/* cp.c — submission (under a VM's VMID) */
int	 amd_submit(struct atrium_amd_softc *sc, struct atrium_amd_bo *ring,
	    uint32_t n_dwords, uint32_t engine, uint16_t vmid);
int	 amd_queue_program(struct atrium_amd_softc *sc, uint64_t ring_va,
	    uint32_t engine, uint16_t vmid, uint32_t *doorbell_off);

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

/* irq.c — MSI-X interrupt setup, the ISR, and the pending-completion list */
int	 amd_irq_setup(struct atrium_amd_softc *sc);
void	 amd_irq_teardown(struct atrium_amd_softc *sc);
void	 amd_pending_push(struct atrium_amd_softc *sc,
	    struct atrium_amd_syncobj *so, uint64_t value);
void	 amd_pending_scrub(struct atrium_amd_softc *sc,
	    struct atrium_amd_syncobj *so);

/* ioctl.c — the cdev character-device switch */
extern struct cdevsw atrium_amd_cdevsw;

#endif /* _ATRIUM_GPU_AMD_H_ */
