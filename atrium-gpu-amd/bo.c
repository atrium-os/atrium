/*
 * bo.c — DMA-page allocator + buffer-object handle table.
 *
 * The memory substrate the rest of the driver builds on: pages of DMA-able
 * guest RAM (page tables, rings, IH, and userspace BOs all come from here),
 * plus the handle table that names the BOs userspace allocates and maps into
 * GPUVM.
 */
#include "atrium_gpu_amd.h"

/*
 * Allocate one page of DMA-able guest memory and register it. Returns the
 * kernel VA (CPU side) and, via *gpa_out, the guest-physical address the
 * device DMA-walks.
 *
 * WHY contigmalloc + vtophys, not busdma: gpusim runs in a VM with no IOMMU on
 * this device, so the guest-physical address IS the address the model's DMA
 * backend (QEMU pci_dma_read/write) uses — vtophys() of a page-aligned page
 * gives exactly that. Real silicon will need bus_dma tags + bus_dmamap_sync
 * (IOMMU translation + cache maintenance) and multi-page BOs; both are
 * deliberately deferred so this stays a clean model-driven bring-up.
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
 * Map a registered DMA page's guest-physical address back to its kernel VA —
 * needed when the GPUVM walker reuses a page-table page we already allocated
 * (we hold the PDE's phys, but must write PTEs through the CPU mapping).
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

/*
 * Allocate a buffer object: a page of DMA memory placed at the next GPU-VA and
 * mapped into the kernel context's page table. Returns a handle (its index)
 * and the GPU-VA. One page per BO for now (size capped); multi-page BOs land
 * with the bus_dma rework.
 */
int
amd_bo_alloc(struct atrium_amd_softc *sc, uint64_t size, uint32_t *handle_out,
    uint64_t *gpu_va_out)
{
	struct atrium_amd_bo *bo;
	vm_paddr_t gpa;
	void *kva;
	uint64_t va;
	int err;

	if (size == 0 || size > PAGE_SIZE)
		return (EINVAL);
	if (sc->n_bo >= ATRIUM_AMD_MAX_BO)
		return (ENOSPC);

	kva = amd_dma_alloc(sc, &gpa);
	if (kva == NULL)
		return (ENOMEM);

	va = sc->next_gpu_va;
	err = amd_gpuvm_map(sc, va, gpa);
	if (err != 0)
		return (err);	/* page already in dma[]; freed at teardown */
	sc->next_gpu_va += PAGE_SIZE;

	bo = &sc->bo[sc->n_bo];
	bo->kva = kva;
	bo->gpu_va = va;
	bo->size = size;
	*handle_out = (uint32_t)sc->n_bo;
	*gpu_va_out = va;
	sc->n_bo++;
	return (0);
}

/* Resolve a userspace handle to its BO, or NULL if out of range. */
struct atrium_amd_bo *
amd_bo_lookup(struct atrium_amd_softc *sc, uint32_t handle)
{
	if (handle >= (uint32_t)sc->n_bo)
		return (NULL);
	return (&sc->bo[handle]);
}
