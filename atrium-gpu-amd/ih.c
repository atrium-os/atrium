/*
 * ih.c — the device-global interrupt handler (IH) ring + ISR, owned by the BASE
 * (pci) module.
 *
 * On real AMD silicon the IH ring is a DEVICE resource, not a GPU-block one:
 * GFX end-of-pipe, SDMA, and DCN vblank/pageflip all push cookies into the SAME
 * ring, and the whole device shares one (or a few) MSI-X vectors. A single ISR
 * drains the ring and demuxes by the cookie's source/cause, dispatching to each
 * IP block's registered handler (amdgpu's amdgpu_ih.c + amdgpu_irq_dispatch).
 *
 * So the ring + the ISR live HERE, in the device owner — not in the gpu module.
 * Each IP module registers a handler for its own cause via amd_ih_set_handler:
 * the gpu registers IH_CAUSE_EOP (fence retire), the display registers
 * IH_CAUSE_VBLANK (vblank knote). Neither depends on the other, and vblank is
 * delivered even with the gpu (render) module unloaded — the §4.1 independence
 * the split promises.
 */
#include "atrium_gpu_amd.h"

#include <machine/atomic.h>

/* --- IH ring backing: one coherent DMA page the device write-walks --------- */

/* bus_dmamap_load callback: capture the single page's bus address. */
static void
amd_ih_load_cb(void *arg, bus_dma_segment_t *segs, int nseg, int error)
{
	bus_addr_t *out = arg;

	if (error == 0 && nseg == 1)
		*out = segs[0].ds_addr;
}

/*
 * Stand up the device-global IH ring: allocate one coherent page through
 * bus_dma(9) (IOMMU-ready bus address, the real-silicon path) and program its
 * base + size so the device has somewhere to write interrupt cookies. The base
 * owns this DMA page directly (its own tag/map in the shared softc) rather than
 * borrowing the gpu module's page allocator — the ring outlives any GPU.
 */
int
amd_ih_init(struct atrium_amd_softc *sc)
{
	bus_addr_t ih_gpa = 0;
	int err;

	err = bus_dma_tag_create(bus_get_dma_tag(sc->dev), PAGE_SIZE, 0,
	    BUS_SPACE_MAXADDR, BUS_SPACE_MAXADDR, NULL, NULL,
	    PAGE_SIZE, 1, PAGE_SIZE, 0, NULL, NULL, &sc->ih_tag);
	if (err != 0)
		return (err);
	err = bus_dmamem_alloc(sc->ih_tag, &sc->ih_kva,
	    BUS_DMA_WAITOK | BUS_DMA_ZERO | BUS_DMA_COHERENT, &sc->ih_map);
	if (err != 0) {
		bus_dma_tag_destroy(sc->ih_tag);
		sc->ih_tag = NULL;
		return (err);
	}
	err = bus_dmamap_load(sc->ih_tag, sc->ih_map, sc->ih_kva, PAGE_SIZE,
	    amd_ih_load_cb, &ih_gpa, BUS_DMA_NOWAIT);
	if (err != 0 || ih_gpa == 0) {
		bus_dmamem_free(sc->ih_tag, sc->ih_kva, sc->ih_map);
		bus_dma_tag_destroy(sc->ih_tag);
		sc->ih_kva = NULL;
		sc->ih_tag = NULL;
		return (err != 0 ? err : EIO);
	}

	sc->ih_rptr = 0;
	amd_mmio_write32(sc, regIH_BASE_LO, (uint32_t)(ih_gpa & 0xffffffff));
	amd_mmio_write32(sc, regIH_BASE_HI, (uint32_t)(ih_gpa >> 32));
	amd_mmio_write32(sc, regIH_SIZE, ATRIUM_AMD_IH_ENTRIES);
	return (0);
}

void
amd_ih_fini(struct atrium_amd_softc *sc)
{
	if (sc->ih_kva != NULL) {
		bus_dmamap_unload(sc->ih_tag, sc->ih_map);
		bus_dmamem_free(sc->ih_tag, sc->ih_kva, sc->ih_map);
		sc->ih_kva = NULL;
	}
	if (sc->ih_tag != NULL) {
		bus_dma_tag_destroy(sc->ih_tag);
		sc->ih_tag = NULL;
	}
	sc->ih_rptr = 0;
}

/* --- the ISR: drain the ring, demux by cause, dispatch to handlers --------- */

/*
 * MSI-X is message-signaled (edge), so there is no level to acknowledge —
 * reading the new cookies is the whole handshake. We drain from our read pointer
 * up to the device's write pointer, tallying each cookie by cause, then call the
 * registered handler for every cause that fired. The whole thing runs under
 * sc->lock: the handlers expect it held (the EOP retire nests so->lock under it;
 * the vblank KNOTE_LOCKED requires the knlist lock, which IS sc->lock), and it
 * serializes dispatch against amd_ih_set_handler so a detaching module can clear
 * its handler with no chance of the ISR calling into unloaded text.
 */
static void
amd_intr(void *arg)
{
	struct atrium_amd_softc *sc = arg;
	int counts[ATRIUM_AMD_IH_NCAUSE];
	uint32_t wptr, idx, cause;
	const uint32_t *cookie;
	int c;

	for (c = 0; c < ATRIUM_AMD_IH_NCAUSE; c++)
		counts[c] = 0;

	mtx_lock(&sc->lock);
	wptr = amd_mmio_read32(sc, regIH_WPTR);
	while (sc->ih_rptr != wptr) {
		idx = sc->ih_rptr % ATRIUM_AMD_IH_ENTRIES;
		cookie = (const uint32_t *)((const char *)sc->ih_kva +
		    idx * ATRIUM_AMD_IH_COOKIE);
		cause = cookie[0];
		if (cause < ATRIUM_AMD_IH_NCAUSE)
			counts[cause]++;
		sc->ih_rptr++;
	}
	for (c = 0; c < ATRIUM_AMD_IH_NCAUSE; c++) {
		if (counts[c] > 0 && sc->ih_handler[c] != NULL)
			sc->ih_handler[c](sc, counts[c]);
	}
	mtx_unlock(&sc->lock);

	/* Observable that an interrupt reached the guest (GET_IRQS), independent of
	 * cause — used by the vblank-rate and fence smokes. */
	atomic_add_int(&sc->irq_count, 1);
}

/*
 * Allocate MSI-X vector 0's IRQ resource (the base already called
 * pci_alloc_msix as the device owner) and hook the ISR. Non-fatal on failure:
 * the device still works via the synchronous-drain path, so the caller logs and
 * runs in poll mode rather than failing attach.
 */
int
amd_irq_setup(struct atrium_amd_softc *sc)
{
	if (sc->msix_table == NULL)
		return (ENXIO);

	sc->irq_rid = 1;	/* MSI-X vector 0 is resource id 1 */
	sc->irq = bus_alloc_resource_any(sc->dev, SYS_RES_IRQ, &sc->irq_rid,
	    RF_ACTIVE);
	if (sc->irq == NULL)
		return (ENXIO);
	if (bus_setup_intr(sc->dev, sc->irq, INTR_TYPE_MISC | INTR_MPSAFE,
	    NULL, amd_intr, sc, &sc->intr_cookie) != 0) {
		bus_release_resource(sc->dev, SYS_RES_IRQ, sc->irq_rid, sc->irq);
		sc->irq = NULL;
		return (ENXIO);
	}
	sc->msix_enabled = 1;
	return (0);
}

/* Tear down the interrupt hookup (safe whether or not setup succeeded). */
void
amd_irq_teardown(struct atrium_amd_softc *sc)
{
	if (sc->intr_cookie != NULL) {
		bus_teardown_intr(sc->dev, sc->irq, sc->intr_cookie);
		sc->intr_cookie = NULL;
	}
	if (sc->irq != NULL) {
		bus_release_resource(sc->dev, SYS_RES_IRQ, sc->irq_rid, sc->irq);
		sc->irq = NULL;
	}
	/* The MSI-X table BAR + pci_release_msi belong to the base's attach/detach. */
	sc->msix_enabled = 0;
}
