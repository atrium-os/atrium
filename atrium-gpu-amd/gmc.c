/*
 * gmc.c — Graphics Memory Controller bring-up: enable GPUVM paging and stand
 * up the interrupt ring. The page tables themselves are per-process now (one
 * per VM, vm.c) — there is no global kernel page directory; each VM programs
 * its own page-directory base via the per-context registers.
 */
#include "atrium_gpu_amd.h"

int
amd_gmc_init(struct atrium_amd_softc *sc)
{
	vm_paddr_t ih_gpa;

	/* Enable GPUVM translation; per-VMID page-directory bases are set per VM. */
	amd_mmio_write32(sc, regGMC_ENABLE, 1);

	/*
	 * IH init: stand up the interrupt-handler ring so the CP's end-of-pipe
	 * RELEASE_MEM has somewhere to write its cookie (consumed by the ISR).
	 */
	sc->ih_kva = amd_dma_alloc(sc, &ih_gpa);
	if (sc->ih_kva == NULL) {
		device_printf(sc->dev, "GMC: failed to allocate IH ring\n");
		return (ENOMEM);
	}
	amd_mmio_write32(sc, regIH_BASE_LO, (uint32_t)(ih_gpa & 0xffffffff));
	amd_mmio_write32(sc, regIH_BASE_HI, (uint32_t)(ih_gpa >> 32));
	amd_mmio_write32(sc, regIH_SIZE, ATRIUM_AMD_IH_ENTRIES);
	return (0);
}
