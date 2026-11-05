/*
 * bo.c — DMA pages + fd-backed buffer objects.
 *
 * `amd_dma_page_alloc`/`_free` are the per-page bus_dma(9) primitive the GPUVM
 * page tables are built from (vm.c). (The device-global IH ring has its own
 * page, allocated by the base module in ih.c.) Buffer *objects* are the
 * resources userspace owns: each is a `struct file` (fd-as-handle), owns its own
 * page, is mapped into a VM's GPUVM at allocation, and holds a reference on that
 * VM so it can unmap on close. The integer handle table is gone.
 */
#include "atrium_gpu_amd.h"
#include "atrium_gpu_amd_abi.h"	/* ATRIUM_GPU_BO_VRAM placement flag */

/*
 * bus_dmamap_load callback: record each page's bus address. With a tag whose
 * maxsegsz == PAGE_SIZE the loader hands us one segment per page, so the GPUVM
 * can map a PTE per page (scatter-gather: backing pages need not be contiguous).
 */
struct amd_bo_load {
	bus_addr_t	*pages;
	int		 npages;	/* segments stored (out) */
	int		 max;		/* capacity of pages[] (in) */
	int		 error;
};

static void
amd_bo_load_cb(void *arg, bus_dma_segment_t *segs, int nseg, int error)
{
	struct amd_bo_load *l = arg;
	int i;

	l->error = error;
	if (error != 0)
		return;
	/* Bound by the CALLER's array, not a global maximum: pages[] is sized
	 * to its BO now, and this callback also serves the single-page helper. */
	for (i = 0; i < nseg && i < l->max; i++)
		l->pages[i] = segs[i].ds_addr;
	l->npages = nseg;
}

/*
 * Allocate one page of DMA-able memory through bus_dma(9) — the real-silicon
 * path (IOMMU-ready bus address, not vtophys), used for every device-walked
 * internal page (the IH ring and each VM's page-directory/page-table pages).
 * Coherent + page-granular, so p->gpa is the page's bus address.
 */
int
amd_dma_page_alloc(struct atrium_amd_softc *sc, struct atrium_amd_dma_page *p)
{
	struct amd_bo_load load;
	bus_addr_t seg;
	int err;

	err = bus_dma_tag_create(bus_get_dma_tag(sc->dev), PAGE_SIZE, 0,
	    BUS_SPACE_MAXADDR, BUS_SPACE_MAXADDR, NULL, NULL,
	    PAGE_SIZE, 1, PAGE_SIZE, 0, NULL, NULL, &p->tag);
	if (err != 0)
		return (err);
	err = bus_dmamem_alloc(p->tag, &p->kva,
	    BUS_DMA_WAITOK | BUS_DMA_ZERO | BUS_DMA_COHERENT, &p->map);
	if (err != 0) {
		bus_dma_tag_destroy(p->tag);
		p->tag = NULL;
		return (err);
	}
	load.pages = &seg;
	load.npages = 0;
	load.max = 1;		/* &seg is ONE entry — the callback must not run past it */
	load.error = 0;
	err = bus_dmamap_load(p->tag, p->map, p->kva, PAGE_SIZE, amd_bo_load_cb,
	    &load, BUS_DMA_NOWAIT);
	if (err != 0 || load.error != 0 || load.npages != 1) {
		bus_dmamem_free(p->tag, p->kva, p->map);
		bus_dma_tag_destroy(p->tag);
		p->kva = NULL;
		p->tag = NULL;
		return (err != 0 ? err : EIO);
	}
	p->gpa = seg;
	return (0);
}

void
amd_dma_page_free(struct atrium_amd_dma_page *p)
{
	if (p->kva != NULL) {
		bus_dmamap_unload(p->tag, p->map);
		bus_dmamem_free(p->tag, p->kva, p->map);
		p->kva = NULL;
	}
	if (p->tag != NULL) {
		bus_dma_tag_destroy(p->tag);
		p->tag = NULL;
	}
}

/* --- buffer objects: fd-backed, mapped into a VM --- */

/*
 * Reclaim a BO: unmap every page from its VM, drop the VM reference, release the
 * DMA mapping/memory/tag and the struct. Safe before the mapping/VM-ref are
 * established (an unbound BO has vm == NULL; a NULL vm_fp is skipped).
 */
static void
amd_bo_destroy(struct atrium_amd_bo *bo, struct thread *td)
{
	struct atrium_amd_softc *sc = bo->sc;
	uint16_t vmids[ATRIUM_AMD_BO_MAX_BIND];
	uint64_t gen[ATRIUM_AMD_NHWQ];
	uint32_t busy;
	int b, i, n = 0;

	/* Clear the BO's PTEs in every VM it is bound into (each unmap flushes
	 * the TLB), so no new access can reach its pages. */
	for (b = 0; b < bo->n_bindings; b++) {
		struct atrium_amd_bo_binding *bd = &bo->bindings[b];

		for (i = 0; i < bo->npages; i++)
			sc->backend->unmap_page(bd->vm,
			    bd->gpu_va + (uint64_t)i * PAGE_SIZE);
		vmids[n++] = bd->vm->vmid;
		/* The PTEs are gone, so the range can go back to the VM's arena
		 * now (next-fit hands it out again only after a full lap). */
		vmem_free(bd->vm->va_arena, bd->gpu_va,
		    (vmem_size_t)bo->npages * PAGE_SIZE);
	}
	/* Work already queued under one of those VMs may still be using the
	 * pages (isolation break 2): note which queues are busy now, before
	 * dropping the VM references (the last one tears the VM down, which
	 * stops its queues). */
	busy = amd_hwq_busy_snapshot(sc, vmids, n, gen);
	for (b = 0; b < bo->n_bindings; b++)
		fdrop(bo->bindings[b].vm_fp, td);
	bo->n_bindings = 0;
	if (busy != 0) {
		amd_hwq_defer_bo(sc, bo, busy, gen);	/* freed once drained */
		return;
	}
	sc->backend->bo_free(bo);	/* backing store + bo_count-- */
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
 * amd backend: allocate a BO's backing store into a (front-end-owned) bo.
 * VRAM-resident BOs carve device-local pages from vram.c (no bus_dma / no CPU map —
 * VRAM is GPU-only; userspace populates it with a GPU copy from a System staging
 * BO; pages[] hold VRAM offsets). System BOs go through bus_dma(9) — the real-
 * silicon path (proper bus addresses, IOMMU-ready), one page-granular segment
 * per page for the GPUVM to map. Self-cleaning on failure; fills pages/size and
 * bumps bo_count on success.
 */
int
amd_bo_backing_alloc(struct atrium_amd_softc *sc, struct atrium_amd_bo *bo,
    uint64_t size, uint32_t flags)
{
	struct amd_bo_load load;
	bus_dma_tag_t dmat;
	bus_dmamap_t dmamap;
	void *kva;
	uint64_t rounded;
	int err, npages;

	if (size == 0 || size > (uint64_t)ATRIUM_AMD_BO_MAX_PAGES * PAGE_SIZE)
		return (EINVAL);
	rounded = round_page(size);
	npages = rounded / PAGE_SIZE;

	/* Page list sized to THIS BO. It used to be an inline array in every
	 * struct, which is what forced a 2 MiB ceiling on all BOs and capped
	 * the display at VGA (see ATRIUM_AMD_BO_MAX_PAGES). */
	bo->pages = malloc((size_t)npages * sizeof(bus_addr_t), M_DEVBUF,
	    M_WAITOK | M_ZERO);

	if ((flags & ATRIUM_GPU_BO_VRAM) != 0) {
		uint64_t vram_off;
		int i;

		/* Carve from the device VRAM allocator (base-owned, vram.c):
		 * clean VRAM only, never what another client left behind. */
		if (amd_vram_alloc(sc, rounded, 0, &vram_off) != 0) {
			free(bo->pages, M_DEVBUF);
			bo->pages = NULL;
			return (ENOMEM);
		}
		mtx_lock(&sc->lock);
		sc->bo_count++;
		mtx_unlock(&sc->lock);
		bo->vram = 1;
		bo->npages = npages;
		bo->size = size;
		for (i = 0; i < npages; i++)
			bo->pages[i] = vram_off + (uint64_t)i * PAGE_SIZE;
		return (0);
	}

	err = bus_dma_tag_create(bus_get_dma_tag(sc->dev),
	    PAGE_SIZE, 0,			/* alignment, boundary */
	    BUS_SPACE_MAXADDR, BUS_SPACE_MAXADDR, NULL, NULL,
	    rounded, npages, PAGE_SIZE,	/* maxsize, nsegments, maxsegsz */
	    0, NULL, NULL, &dmat);
	if (err != 0) {
		free(bo->pages, M_DEVBUF);
		bo->pages = NULL;
		return (err);
	}
	err = bus_dmamem_alloc(dmat, &kva,
	    BUS_DMA_WAITOK | BUS_DMA_ZERO | BUS_DMA_COHERENT, &dmamap);
	if (err != 0) {
		bus_dma_tag_destroy(dmat);
		free(bo->pages, M_DEVBUF);
		bo->pages = NULL;
		return (ENOMEM);
	}
	bo->kva = kva;
	bo->dmat = dmat;
	bo->dmamap = dmamap;
	bo->size = size;

	load.pages = bo->pages;
	load.npages = 0;
	load.max = npages;
	load.error = 0;
	err = bus_dmamap_load(dmat, dmamap, kva, rounded, amd_bo_load_cb, &load,
	    BUS_DMA_NOWAIT);
	if (err != 0 || load.error != 0 || load.npages != npages) {
		bus_dmamem_free(dmat, kva, dmamap);
		bus_dma_tag_destroy(dmat);
		bo->kva = NULL;
		bo->dmat = NULL;
		free(bo->pages, M_DEVBUF);
		bo->pages = NULL;
		return (err != 0 ? err : EIO);
	}
	bo->npages = load.npages;

	mtx_lock(&sc->lock);
	sc->bo_count++;
	mtx_unlock(&sc->lock);
	return (0);
}

/*
 * amd backend: export a BO as a scanout handle — the absolute VRAM offset +
 * size the display module imports (dma-buf-equivalent). Only VRAM is scannable;
 * System/GTT BOs have no contiguous VRAM offset.
 */
int
amd_export_scanout(struct atrium_amd_bo *bo, uint64_t *vram_offset,
    uint64_t *size)
{
	if (!bo->vram)
		return (EINVAL);
	*vram_offset = bo->pages[0];
	*size = bo->size;
	return (0);
}

/* amd backend: release a BO's backing store and drop the device's BO count.
 * VRAM goes back to the device allocator, which scrubs it before anyone can
 * have it again; we only get here once no queue can still reach it. */
void
amd_bo_backing_free(struct atrium_amd_bo *bo)
{
	struct atrium_amd_softc *sc = bo->sc;

	mtx_lock(&sc->lock);
	sc->bo_count--;
	mtx_unlock(&sc->lock);
	if (bo->vram && bo->pages != NULL)
		amd_vram_free(sc, bo->pages[0], (uint64_t)bo->npages * PAGE_SIZE);
	if (bo->kva != NULL) {
		bus_dmamap_unload(bo->dmat, bo->dmamap);
		bus_dmamem_free(bo->dmat, bo->kva, bo->dmamap);
		bo->kva = NULL;
	}
	if (bo->dmat != NULL) {
		bus_dma_tag_destroy(bo->dmat);
		bo->dmat = NULL;
	}
	free(bo->pages, M_DEVBUF);	/* free(NULL) is a no-op */
	bo->pages = NULL;
}

/*
 * Create a buffer object and return it as an fd. Transport-neutral front-end:
 * owns the struct + fd object (and later the cross-VM bindings, ABI-v2 principle
 * 4 — a BO is independent of any VM); the backing store comes from the backend.
 */
int
amd_bo_create_fd(struct atrium_amd_softc *sc, struct thread *td, uint64_t size,
    uint32_t flags, int *out_fd)
{
	struct atrium_amd_bo *bo;
	struct file *fp;
	int fd, err;

	bo = malloc(sizeof(*bo), M_DEVBUF, M_WAITOK | M_ZERO);
	bo->sc = sc;
	err = sc->backend->bo_alloc(sc, bo, size, flags);
	if (err != 0) {
		free(bo, M_DEVBUF);	/* bo_alloc unwound its own backing */
		return (err);
	}

	err = falloc_noinstall(td, &fp);
	if (err != 0) {
		amd_bo_destroy(bo, td);
		return (err);
	}
	finit(fp, FREAD | FWRITE, DTYPE_DEV, bo, &atrium_amd_bo_fileops);
	err = finstall(td, fp, &fd, 0, NULL);
	fdrop(fp, td);
	if (err != 0)
		return (err);	/* fo_close already reclaimed the BO */

	*out_fd = fd;
	return (0);
}

/*
 * Bind a BO into `vm` at *va (0 = pick the VM's next bump VA), adding one more
 * binding (a BO may be bound into up to ATRIUM_AMD_BO_MAX_BIND VMs at once — the
 * sharing path). On success the new binding takes over `vm_fp` (released when
 * the BO is freed, so every bound VM outlives it); on error the caller keeps it.
 */
uint64_t
amd_bo_gpu_va(struct atrium_amd_bo *bo, struct atrium_amd_vm *vm)
{
	int b;

	for (b = 0; b < bo->n_bindings; b++)
		if (bo->bindings[b].vm == vm)
			return (bo->bindings[b].gpu_va);
	return (0);	/* not bound in this vm */
}

int
amd_bo_bind(struct atrium_amd_bo *bo, struct atrium_amd_vm *vm,
    struct file *vm_fp, uint64_t *va, uint32_t bind_flags)
{
	struct atrium_amd_softc *sc = bo->sc;
	const uint64_t span = (uint64_t)bo->npages * PAGE_SIZE;
	vmem_addr_t addr;
	int i, err;

	if ((bind_flags & ~ATRIUM_GPU_BIND_READONLY) != 0)
		return (EINVAL);
	if (bo->n_bindings >= ATRIUM_AMD_BO_MAX_BIND)
		return (ENOSPC);	/* shared into too many VMs */
	/* A BO maps at most once per VM: a second bind into the same VM would
	 * alias it at two VAs and leak the first binding's vm_fp reference. */
	for (i = 0; i < bo->n_bindings; i++)
		if (bo->bindings[i].vm == vm)
			return (EBUSY);

	/*
	 * Reserve the range in the VM's arena. A caller-chosen VA must be
	 * page-aligned, inside the window and clear of every other binding
	 * (review §A2.5); otherwise next-fit picks one, reusing freed ranges.
	 */
	if (*va != 0) {
		if ((*va & PAGE_MASK) != 0 || *va + span < *va)
			return (EINVAL);
		if (vmem_xalloc(vm->va_arena, span, 0, 0, 0, *va, *va + span,
		    M_BESTFIT | M_NOWAIT, &addr) != 0)
			return (EEXIST);	/* overlaps a binding or off-window */
		if (addr != *va) {
			vmem_free(vm->va_arena, addr, span);
			return (EEXIST);
		}
	} else if (vmem_alloc(vm->va_arena, span, M_NEXTFIT | M_NOWAIT,
	    &addr) != 0) {
		return (ENOSPC);
	}

	/* One PTE per page — the GPUVM gathers the BO's (possibly scattered) pages
	 * into a contiguous GPU-VA range. Each VM has its own page tables, so the
	 * same pages can map at independent VAs in different VMs. */
	for (i = 0; i < bo->npages; i++) {
		err = sc->backend->map_page(vm, addr + (uint64_t)i * PAGE_SIZE,
		    bo->pages[i], bo->vram,
		    (bind_flags & ATRIUM_GPU_BIND_READONLY) == 0);
		if (err != 0) {
			while (i-- > 0)
				sc->backend->unmap_page(vm,
				    addr + (uint64_t)i * PAGE_SIZE);
			vmem_free(vm->va_arena, addr, span);
			return (err);
		}
	}

	bo->bindings[bo->n_bindings].vm = vm;
	bo->bindings[bo->n_bindings].vm_fp = vm_fp;
	bo->bindings[bo->n_bindings].gpu_va = addr;
	bo->n_bindings++;
	*va = addr;
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
