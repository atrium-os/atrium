/*
 * gmc.c — Graphics Memory Controller: GPUVM page tables + GMC/IH bring-up.
 *
 * Owns the device's view of memory: the 2-level page table the GMC DMA-walks
 * to translate GPU virtual addresses, and the one-time GMC/IH register
 * programming that turns paging on and gives the CP an interrupt ring.
 */
#include "atrium_gpu_amd.h"

/*
 * Edit the 2-level GPUVM page table to map GPU-VA `va` -> guest-physical
 * `phys`, allocating a page-table page on demand, then flush the GPU TLB so
 * the device does not walk a stale cached translation (referee INV-GMC-0004).
 * Mirrors the model's walk (residency.rs): PDE at pdb[va>>21], PTE at
 * pt[va>>12], each = (page-aligned phys) | PTE_VALID.
 */
int
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
 * GMC init: allocate the GPUVM page-directory base for the kernel context
 * (VMID 0), program it, enable paging, and stand up the interrupt-handler
 * ring. After this the device DMA-walks the page tables amd_gpuvm_map builds,
 * and end-of-pipe RELEASE_MEM has an IH ring to write its cookie into.
 *
 * (No ISR installed yet — the submit path reads fences directly; the IRQ path
 * is exercised but its delivery is not consumed until the MSI-X milestone.)
 */
int
amd_gmc_init(struct atrium_amd_softc *sc)
{
	vm_paddr_t ih_gpa;

	sc->pdb_kva = amd_dma_alloc(sc, &sc->pdb_gpa);
	if (sc->pdb_kva == NULL) {
		device_printf(sc->dev, "GMC: failed to allocate page-directory\n");
		return (ENOMEM);
	}
	amd_mmio_write32(sc, regPT_BASE_LO, (uint32_t)(sc->pdb_gpa & 0xffffffff));
	amd_mmio_write32(sc, regPT_BASE_HI, (uint32_t)(sc->pdb_gpa >> 32));
	amd_mmio_write32(sc, regGMC_ENABLE, 1);

	sc->ih_kva = amd_dma_alloc(sc, &ih_gpa);
	if (sc->ih_kva == NULL) {
		device_printf(sc->dev, "GMC: failed to allocate IH ring\n");
		return (ENOMEM);
	}
	amd_mmio_write32(sc, regIH_BASE_LO, (uint32_t)(ih_gpa & 0xffffffff));
	amd_mmio_write32(sc, regIH_BASE_HI, (uint32_t)(ih_gpa >> 32));
	amd_mmio_write32(sc, regIH_SIZE, ATRIUM_AMD_IH_ENTRIES);

	sc->next_gpu_va = ATRIUM_AMD_BO_VA_BASE;
	return (0);
}
