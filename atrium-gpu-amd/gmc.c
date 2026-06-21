/*
 * gmc.c — Graphics Memory Controller bring-up: enable GPUVM paging. The page
 * tables themselves are per-process (one per VM, vm.c) — there is no global
 * kernel page directory; each VM programs its own page-directory base via the
 * per-context registers.
 *
 * The interrupt-handler (IH) ring is NOT set up here: it is a device-global
 * resource owned by the base (pci) module (ih.c, amd_ih_init), not the GMC.
 */
#include "atrium_gpu_amd.h"

int
amd_gmc_init(struct atrium_amd_softc *sc)
{
	/* Enable GPUVM translation; per-VMID page-directory bases are set per VM. */
	amd_mmio_write32(sc, regGMC_ENABLE, 1);
	return (0);
}
