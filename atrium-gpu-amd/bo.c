/*
 * bo.c — internal DMA pages + fd-backed buffer objects.
 *
 * Two memory roles live here. `amd_dma_alloc`/`amd_dma_kva` back the driver's
 * *internal* pages — page-table pages and the IH ring — which live in the
 * softc's fixed dma[] registry and are freed at teardown. Buffer *objects*,
 * by contrast, are the resources userspace owns: each is a `struct file`
 * (fd-as-handle, ABI-v2 principle 2), owns its own page, and is unmapped +
 * freed when its last fd reference closes. The integer handle table is gone.
 */
#include "atrium_gpu_amd.h"

/*
 * Allocate one page of DMA-able guest memory for *internal* use (page tables,
 * IH ring) and register it. Returns the kernel VA and, via *gpa_out, the
 * guest-physical address the device DMA-walks.
 *
 * WHY contigmalloc + vtophys, not busdma: gpusim runs in a VM with no IOMMU on
 * this device, so the guest-physical address IS the address the model's DMA
 * backend (QEMU pci_dma_read/write) uses. Real silicon needs bus_dma tags +
 * bus_dmamap_sync; deferred with multi-page BOs.
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

/*
 * Map a registered internal DMA page's guest-physical address back to its
 * kernel VA — needed when the GPUVM walker reuses a page-table page (we hold
 * the PDE's phys but must write PTEs through the CPU mapping).
 */
void *
amd_dma_kva(struct atrium_amd_softc *sc, vm_paddr_t gpa)
{
	int i;

	for (i = 0; i < sc->n_dma; i++)
		if (sc->dma[i].gpa == gpa)
			return (sc->dma[i].kva);
	return (NULL);
}

/* --- buffer objects: fd-backed (lifetime = fd refcount) --- */

/* Unmap a BO from GPUVM, free its page and the struct, drop the live count. */
static void
amd_bo_destroy(struct atrium_amd_bo *bo)
{
	struct atrium_amd_softc *sc = bo->sc;

	mtx_lock(&sc->lock);
	amd_gpuvm_unmap(sc, bo->gpu_va);
	sc->bo_count--;
	mtx_unlock(&sc->lock);
	free(bo->kva, M_DEVBUF);
	free(bo, M_DEVBUF);
}

/* fo_close: the last fd reference to this BO went away — reclaim it. */
static int
amd_bo_close(struct file *fp, struct thread *td)
{
	struct atrium_amd_bo *bo = fp->f_data;

	if (bo != NULL) {
		fp->f_data = NULL;
		amd_bo_destroy(bo);
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
 * Allocate a buffer object and return it as a file descriptor in the caller's
 * fd table, plus its GPU virtual address. One page per BO for now (size
 * capped); multi-page BOs land with the bus_dma rework. The page is allocated
 * before the lock (M_WAITOK may sleep); the lock covers only the page-table
 * edit and the VA bump.
 */
int
amd_bo_create_fd(struct atrium_amd_softc *sc, struct thread *td, uint64_t size,
    int *out_fd, uint64_t *out_gpu_va)
{
	struct atrium_amd_bo *bo;
	struct file *fp;
	void *kva;
	uint64_t va;
	int fd, err;

	if (size == 0 || size > PAGE_SIZE)
		return (EINVAL);

	kva = contigmalloc(PAGE_SIZE, M_DEVBUF, M_WAITOK | M_ZERO, 0,
	    BUS_SPACE_MAXADDR, PAGE_SIZE, 0);
	if (kva == NULL)
		return (ENOMEM);
	bo = malloc(sizeof(*bo), M_DEVBUF, M_WAITOK | M_ZERO);
	bo->sc = sc;
	bo->kva = kva;
	bo->gpa = vtophys(kva);
	bo->size = size;

	mtx_lock(&sc->lock);
	va = sc->next_gpu_va;
	err = amd_gpuvm_map(sc, va, bo->gpa);
	if (err == 0) {
		sc->next_gpu_va += PAGE_SIZE;
		sc->bo_count++;
	}
	mtx_unlock(&sc->lock);
	if (err != 0) {
		free(kva, M_DEVBUF);
		free(bo, M_DEVBUF);
		return (err);
	}
	bo->gpu_va = va;

	/*
	 * Wrap the BO in a struct file. After finit() the BO is owned by the
	 * file: if finstall() fails, fdrop() runs fo_close (amd_bo_close ->
	 * amd_bo_destroy), so we must not also free it on that path.
	 */
	err = falloc_noinstall(td, &fp);
	if (err != 0) {
		amd_bo_destroy(bo);
		return (err);
	}
	finit(fp, FREAD | FWRITE, DTYPE_DEV, bo, &atrium_amd_bo_fileops);
	err = finstall(td, fp, &fd, 0, NULL);
	fdrop(fp, td);
	if (err != 0)
		return (err);	/* fo_close already reclaimed the BO */

	*out_fd = fd;
	*out_gpu_va = va;
	return (0);
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
