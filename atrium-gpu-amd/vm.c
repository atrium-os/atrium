/*
 * vm.c — per-process GPU address spaces (ABI-v2 §5.2) + their page tables.
 *
 * Each VM is a struct file with its own VMID (1..15) and 2-level GPUVM page
 * table, programmed into the device's per-context page-directory registers
 * (VM_CTX_*). Two VMs both start their bump allocator at BO_VA_BASE, so the
 * same GPU-VA in different VMs resolves to different physical memory — the
 * per-context isolation the model's residency engine enforces. The page-table
 * pages are pre-allocated at VM_CREATE (one PT page covers 512 BOs in a 2 MiB
 * span), so mapping a BO later never allocates while a lock is held.
 *
 * The fileops are modeled on the in-tree kqueue/eventfd fd objects.
 */
#include "atrium_gpu_amd.h"

static fo_close_t	amd_vm_close;

static int
amd_vm_stat(struct file *fp, struct stat *sb, struct ucred *active_cred)
{
	bzero(sb, sizeof(*sb));
	sb->st_mode = S_IFCHR;
	return (0);
}

static int
amd_vm_fill_kinfo(struct file *fp, struct kinfo_file *kif, struct filedesc *fdp)
{
	kif->kf_type = KF_TYPE_DEV;
	return (0);
}

const struct fileops atrium_amd_vm_fileops = {
	.fo_read = invfo_rdwr,
	.fo_write = invfo_rdwr,
	.fo_truncate = invfo_truncate,
	.fo_ioctl = invfo_ioctl,
	.fo_poll = invfo_poll,
	.fo_kqfilter = invfo_kqfilter,
	.fo_stat = amd_vm_stat,
	.fo_close = amd_vm_close,
	.fo_chmod = invfo_chmod,
	.fo_chown = invfo_chown,
	.fo_sendfile = invfo_sendfile,
	.fo_fill_kinfo = amd_vm_fill_kinfo,
	.fo_cmp = file_kcmp_generic,
	.fo_flags = DFLAG_PASSABLE,
};

/* The page-directory index every BO_VA_BASE-based VA falls under. */
#define AMD_VM_PD_INDEX	\
	(((ATRIUM_AMD_BO_VA_BASE) >> ATRIUM_AMD_PD_SHIFT) & ATRIUM_AMD_PT_MASK)

/* Allocate / free a VMID (1..15), tracking the live-VM count alongside. */
static int
amd_vmid_alloc(struct atrium_amd_softc *sc, uint16_t *vmid_out)
{
	int i, err = ENOSPC;

	mtx_lock(&sc->lock);
	for (i = 1; i < ATRIUM_AMD_MAX_VMID; i++) {
		if ((sc->vmid_bitmap & (1u << i)) == 0) {
			sc->vmid_bitmap |= (1u << i);
			sc->vm_count++;
			*vmid_out = (uint16_t)i;
			err = 0;
			break;
		}
	}
	mtx_unlock(&sc->lock);
	return (err);
}

/*
 * amd backend: release a VM's hardware (VMID + the page-directory/table pages).
 * Idempotent over the page array (NULL-safe). Does NOT free the struct — that
 * is the front-end's (amd_vm_destroy). Undoes a successful amd_vm_setup; also
 * called by amd_vm_setup to unwind its own partial failure.
 */
void
amd_vm_teardown(struct atrium_amd_vm *vm)
{
	struct atrium_amd_softc *sc = vm->sc;
	int i;

	mtx_lock(&sc->lock);
	sc->vmid_bitmap &= ~(1u << vm->vmid);
	sc->vm_count--;
	mtx_unlock(&sc->lock);
	for (i = 0; i < ATRIUM_AMD_VM_NUM_PT; i++)
		amd_dma_page_free(&vm->pt[i]);
	amd_dma_page_free(&vm->pdb);
}

/* Front-end: tear down the hardware (via the backend) and free the struct. */
static void
amd_vm_destroy(struct atrium_amd_vm *vm)
{
	vm->sc->backend->vm_teardown(vm);
	free(vm, M_DEVBUF);
}

/*
 * amd backend: stand up a VM's GPUVM — allocate a VMID, its page-directory and
 * NUM_PT page-table pages (wiring a contiguous run of PDEs), and program the
 * device's per-context PT base. Self-cleaning: on any failure it unwinds what
 * it allocated and returns an error, leaving the struct for the caller to free.
 */
int
amd_vm_setup(struct atrium_amd_softc *sc, struct atrium_amd_vm *vm)
{
	uint64_t *pde;
	uint16_t vmid;
	int err, i;

	err = amd_vmid_alloc(sc, &vmid);
	if (err != 0)
		return (err);
	vm->vmid = vmid;
	if (amd_dma_page_alloc(sc, &vm->pdb) != 0) {
		amd_vm_teardown(vm);
		return (ENOMEM);
	}
	/* A contiguous run of NUM_PT page-table pages, each wired into its own
	 * page-directory entry (PD indices AMD_VM_PD_INDEX .. +NUM_PT-1), so the
	 * bump allocator spans NUM_PT * 2 MiB of VA. */
	for (i = 0; i < ATRIUM_AMD_VM_NUM_PT; i++) {
		if (amd_dma_page_alloc(sc, &vm->pt[i]) != 0) {
			amd_vm_teardown(vm);
			return (ENOMEM);
		}
		pde = (uint64_t *)((char *)vm->pdb.kva +
		    (AMD_VM_PD_INDEX + i) * 8);
		*pde = (vm->pt[i].gpa & ~0xfffULL) | ATRIUM_AMD_PTE_VALID;
	}
	vm->next_va = ATRIUM_AMD_BO_VA_BASE;

	/* Program this VMID's page-directory base into the device. */
	mtx_lock(&sc->lock);
	amd_mmio_write32(sc, regVM_CTX_SELECT, vmid);
	amd_mmio_write32(sc, regVM_CTX_PT_BASE_LO,
	    (uint32_t)(vm->pdb.gpa & 0xffffffff));
	amd_mmio_write32(sc, regVM_CTX_PT_BASE_HI, (uint32_t)(vm->pdb.gpa >> 32));
	mtx_unlock(&sc->lock);
	return (0);
}

static int
amd_vm_close(struct file *fp, struct thread *td)
{
	struct atrium_amd_vm *vm = fp->f_data;

	if (vm != NULL) {
		fp->f_data = NULL;
		amd_vm_destroy(vm);
	}
	return (0);
}

/*
 * Map a BO page into this VM (write its PTE in the pre-allocated page-table
 * page) and flush the TLB. Restricted to the single PT page's 2 MiB span; a VA
 * outside it is EINVAL (the bump allocator never produces one).
 */
int
amd_vm_map(struct atrium_amd_vm *vm, uint64_t va, vm_paddr_t phys, int vram)
{
	struct atrium_amd_softc *sc = vm->sc;
	uint64_t pte;
	uint64_t *pt;
	uint32_t pt_i, pd_i;

	pd_i = (va >> ATRIUM_AMD_PD_SHIFT) & ATRIUM_AMD_PT_MASK;
	if (pd_i < AMD_VM_PD_INDEX ||
	    pd_i >= AMD_VM_PD_INDEX + ATRIUM_AMD_VM_NUM_PT)
		return (EINVAL);
	pt_i = (va >> ATRIUM_AMD_PT_SHIFT) & ATRIUM_AMD_PT_MASK;
	pt = (uint64_t *)vm->pt[pd_i - AMD_VM_PD_INDEX].kva;
	/* phys is a guest-physical (System) addr or a VRAM offset; the PTE_VRAM
	 * bit tells the GMC which backing to walk. */
	pte = ((uint64_t)phys & ~0xfffULL) | ATRIUM_AMD_PTE_VALID;
	if (vram)
		pte |= ATRIUM_AMD_PTE_VRAM;
	mtx_lock(&sc->lock);
	pt[pt_i] = pte;
	amd_mmio_write32(sc, regTLB_INVALIDATE, 1);
	mtx_unlock(&sc->lock);
	return (0);
}

void
amd_vm_unmap(struct atrium_amd_vm *vm, uint64_t va)
{
	struct atrium_amd_softc *sc = vm->sc;
	uint64_t *pt;
	uint32_t pt_i, pd_i;

	pd_i = (va >> ATRIUM_AMD_PD_SHIFT) & ATRIUM_AMD_PT_MASK;
	if (pd_i < AMD_VM_PD_INDEX ||
	    pd_i >= AMD_VM_PD_INDEX + ATRIUM_AMD_VM_NUM_PT)
		return;
	pt_i = (va >> ATRIUM_AMD_PT_SHIFT) & ATRIUM_AMD_PT_MASK;
	pt = (uint64_t *)vm->pt[pd_i - AMD_VM_PD_INDEX].kva;
	mtx_lock(&sc->lock);
	pt[pt_i] = 0;
	amd_mmio_write32(sc, regTLB_INVALIDATE, 1);
	mtx_unlock(&sc->lock);
}

/*
 * Create a VM and return it as an fd. This is the transport-neutral front-end:
 * it owns the struct + the fd object and stands the hardware up via the backend
 * (amd: VMID + GPUVM page tables; virtio: a 3D context).
 */
int
amd_vm_create_fd(struct atrium_amd_softc *sc, struct thread *td, int *out_fd)
{
	struct atrium_amd_vm *vm;
	struct file *fp;
	int fd, err;

	vm = malloc(sizeof(*vm), M_DEVBUF, M_WAITOK | M_ZERO);
	vm->sc = sc;
	err = sc->backend->vm_setup(sc, vm);	/* hardware address space */
	if (err != 0) {
		free(vm, M_DEVBUF);	/* vm_setup unwound its own partial state */
		return (err);
	}

	err = falloc_noinstall(td, &fp);
	if (err != 0) {
		amd_vm_destroy(vm);
		return (err);
	}
	vm->fp = fp;
	finit(fp, FREAD | FWRITE, DTYPE_DEV, vm, &atrium_amd_vm_fileops);
	err = finstall(td, fp, &fd, 0, NULL);
	fdrop(fp, td);
	if (err != 0)
		return (err);	/* fo_close already reclaimed it */

	*out_fd = fd;
	return (0);
}

int
amd_vm_fget(struct thread *td, int fd, struct file **out_fp,
    struct atrium_amd_vm **out_vm)
{
	cap_rights_t rights;
	struct file *fp;
	int err;

	err = fget(td, fd, cap_rights_init(&rights), &fp);
	if (err != 0)
		return (err);
	if (fp->f_ops != &atrium_amd_vm_fileops) {
		fdrop(fp, td);
		return (EINVAL);
	}
	*out_fp = fp;
	*out_vm = fp->f_data;
	return (0);
}
