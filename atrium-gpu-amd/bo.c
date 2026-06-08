/*
 * bo.c — internal DMA pages + fd-backed buffer objects.
 *
 * `amd_dma_alloc` backs the driver's one remaining internal page (the IH ring;
 * page tables are now per-VM, vm.c). Buffer *objects* are the resources
 * userspace owns: each is a `struct file` (fd-as-handle), owns its own page,
 * is mapped into a VM's GPUVM at allocation, and holds a reference on that VM
 * so it can unmap on close. The integer handle table is gone.
 */
#include "atrium_gpu_amd.h"

/*
 * Allocate one page of DMA-able guest memory for internal use and register it.
 * Returns the kernel VA and, via *gpa_out, the guest-physical address the
 * device DMA-walks. (VM/no-IOMMU: gpa == vtophys; real silicon needs bus_dma.)
 */
void *
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

/* --- buffer objects: fd-backed, mapped into a VM --- */

/*
 * Reclaim a BO: unmap it from its VM, drop the VM reference, free the page and
 * the struct. Safe before the mapping/VM-ref are established (gpu_va 0 unmaps
 * nothing; a NULL vm_fp is skipped).
 */
static void
amd_bo_destroy(struct atrium_amd_bo *bo, struct thread *td)
{
	struct atrium_amd_softc *sc = bo->sc;

	if (bo->vm != NULL)
		amd_vm_unmap(bo->vm, bo->gpu_va);
	if (bo->vm_fp != NULL)
		fdrop(bo->vm_fp, td);
	mtx_lock(&sc->lock);
	sc->bo_count--;
	mtx_unlock(&sc->lock);
	free(bo->kva, M_DEVBUF);
	free(bo, M_DEVBUF);
}

static int
amd_bo_close(struct file *fp, struct thread *td)
{
	struct atrium_amd_bo *bo = fp->f_data;

	if (bo != NULL) {
		fp->f_data = NULL;
		amd_bo_destroy(bo, td);
	}
	return (0);
}

static int
amd_bo_stat(struct file *fp, struct stat *sb, struct ucred *active_cred)
{
	bzero(sb, sizeof(*sb));
	sb->st_mode = S_IFCHR;
	return (0);
}

static int
amd_bo_fill_kinfo(struct file *fp, struct kinfo_file *kif, struct filedesc *fdp)
{
	kif->kf_type = KF_TYPE_DEV;
	return (0);
}

const struct fileops atrium_amd_bo_fileops = {
	.fo_read = invfo_rdwr,
	.fo_write = invfo_rdwr,
	.fo_truncate = invfo_truncate,
	.fo_ioctl = invfo_ioctl,
	.fo_poll = invfo_poll,
	.fo_kqfilter = invfo_kqfilter,
	.fo_stat = amd_bo_stat,
	.fo_close = amd_bo_close,
	.fo_chmod = invfo_chmod,
	.fo_chown = invfo_chown,
	.fo_sendfile = invfo_sendfile,
	.fo_fill_kinfo = amd_bo_fill_kinfo,
	.fo_cmp = file_kcmp_generic,
	.fo_flags = DFLAG_PASSABLE,	/* transportable via SCM_RIGHTS */
};

/*
 * Allocate a buffer object inside `vm_fd`'s address space, map it at the VM's
 * next bump VA, and return it as a file descriptor. The BO holds a reference
 * on the VM (so the VM outlives it). The page is allocated before any lock
 * (M_WAITOK may sleep); the VA reservation + bo_count are under sc->lock, and
 * amd_vm_map takes the lock itself for the page-table edit.
 */
int
amd_bo_create_fd(struct atrium_amd_softc *sc, struct thread *td, int vm_fd,
    uint64_t size, int *out_fd, uint64_t *out_gpu_va)
{
	struct atrium_amd_bo *bo;
	struct atrium_amd_vm *vm;
	struct file *fp, *vm_fp;
	const uint64_t va_limit = ATRIUM_AMD_BO_VA_BASE +
	    (uint64_t)ATRIUM_AMD_VM_MAX_BO * PAGE_SIZE;
	void *kva;
	uint64_t va = 0;
	int fd, err;

	if (size == 0 || size > PAGE_SIZE)
		return (EINVAL);

	/* The BO takes over this VM reference (released in amd_bo_destroy). */
	err = amd_vm_fget(td, vm_fd, &vm_fp, &vm);
	if (err != 0)
		return (err);
	kva = contigmalloc(PAGE_SIZE, M_DEVBUF, M_WAITOK | M_ZERO, 0,
	    BUS_SPACE_MAXADDR, PAGE_SIZE, 0);
	if (kva == NULL) {
		fdrop(vm_fp, td);
		return (ENOMEM);
	}
	bo = malloc(sizeof(*bo), M_DEVBUF, M_WAITOK | M_ZERO);
	bo->sc = sc;
	bo->vm = vm;
	bo->vm_fp = vm_fp;
	bo->kva = kva;
	bo->gpa = vtophys(kva);
	bo->size = size;

	mtx_lock(&sc->lock);
	sc->bo_count++;
	if (vm->next_va >= va_limit) {
		mtx_unlock(&sc->lock);
		err = ENOSPC;
		goto fail;
	}
	va = vm->next_va;
	vm->next_va += PAGE_SIZE;
	mtx_unlock(&sc->lock);

	err = amd_vm_map(vm, va, bo->gpa);
	if (err != 0)
		goto fail;
	bo->gpu_va = va;

	err = falloc_noinstall(td, &fp);
	if (err != 0)
		goto fail;
	finit(fp, FREAD | FWRITE, DTYPE_DEV, bo, &atrium_amd_bo_fileops);
	err = finstall(td, fp, &fd, 0, NULL);
	fdrop(fp, td);
	if (err != 0)
		return (err);	/* fo_close already reclaimed the BO */

	*out_fd = fd;
	*out_gpu_va = va;
	return (0);

fail:
	amd_bo_destroy(bo, td);
	return (err);
}

/*
 * Resolve a BO file descriptor to its object, holding a file reference the
 * caller must release with fdrop(*out_fp, td). Rejects fds that are not BOs.
 */
int
amd_bo_fget(struct thread *td, int fd, struct file **out_fp,
    struct atrium_amd_bo **out_bo)
{
	cap_rights_t rights;
	struct file *fp;
	int err;

	err = fget(td, fd, cap_rights_init(&rights), &fp);
	if (err != 0)
		return (err);
	if (fp->f_ops != &atrium_amd_bo_fileops) {
		fdrop(fp, td);
		return (EINVAL);
	}
	*out_fp = fp;
	*out_bo = fp->f_data;
	return (0);
}
