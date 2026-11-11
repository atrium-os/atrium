/*
 * bo.c — DMA pages and the backend half of buffer objects.
 *
 * `amd_dma_page_alloc`/`_free` are the per-page bus_dma(9) primitive the GPUVM
 * page tables are built from (vm.c). (The device-global IH ring has its own
 * page, allocated by the base module in ih.c.) A buffer object's backing store
 * (bus_dma System pages or VRAM) is allocated and freed here; the BO itself,
 * its fd, its bindings into VMs and its lifetime are the Rust front-end's
 * (rs/bo.rs).
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
