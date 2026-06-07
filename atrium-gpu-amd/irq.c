/*
 * irq.c — MSI-X interrupt setup + the ISR that drains the IH ring.
 *
 * Where the earlier milestones relied on the model draining a ring
 * synchronously inside the doorbell write (a model convenience), real silicon
 * completes asynchronously and signals an interrupt. This wires that up: a
 * RELEASE_MEM end-of-pipe event makes the device write a cookie into the IH
 * ring and raise MSI-X vector 0; our ISR drains the new cookies and counts the
 * completion. (Fence-wait/retire built on this is the next step; for now the
 * count is the observable that the interrupt actually reached the guest.)
 */
#include "atrium_gpu_amd.h"

#include <machine/atomic.h>

/*
 * The ISR. Read the device's IH write pointer, drain every cookie from our
 * read pointer up to it (each names the interrupt cause), and record that we
 * serviced an interrupt. MSI-X is message-signaled (edge), so there is no
 * level to acknowledge — reading the new cookies is the whole handshake.
 */
static void
amd_intr(void *arg)
{
	struct atrium_amd_softc *sc = arg;
	uint32_t wptr, idx;
	const uint32_t *cookie;

	wptr = amd_mmio_read32(sc, regIH_WPTR);
	while (sc->ih_rptr != wptr) {
		idx = sc->ih_rptr % ATRIUM_AMD_IH_ENTRIES;
		cookie = (const uint32_t *)((const char *)sc->ih_kva +
		    idx * ATRIUM_AMD_IH_COOKIE);
		(void)cookie[0];	/* cause (IH_CAUSE_EOP); decoded by the */
		(void)cookie[1];	/* fence/retire layer once it exists */
		sc->ih_rptr++;
	}
	atomic_add_int(&sc->irq_count, 1);

	/* Wake any thread blocked in IOC_WAIT_FENCE so it re-tests its fence. */
	mtx_lock(&sc->lock);
	wakeup(&sc->irq_count);
	mtx_unlock(&sc->lock);
}

/*
 * Allocate one MSI-X vector (the model signals vector 0) and hook the ISR.
 * Non-fatal on failure: the device still works via the synchronous-drain path,
 * so the caller logs and continues in poll mode rather than failing attach.
 */
int
amd_irq_setup(struct atrium_amd_softc *sc)
{
	int count = 1;

	/*
	 * pci(9) requires the memory resource holding the MSI-X table to be
	 * allocated before pci_alloc_msix() — otherwise it returns ENXIO. The
	 * table lives in a dedicated BAR (BAR4 for this device); ask the PCI
	 * layer which one rather than hardcoding it.
	 */
	sc->msix_table_rid = pci_msix_table_bar(sc->dev);
	sc->msix_table = bus_alloc_resource_any(sc->dev, SYS_RES_MEMORY,
	    &sc->msix_table_rid, RF_ACTIVE);
	if (sc->msix_table == NULL)
		return (ENXIO);

	if (pci_alloc_msix(sc->dev, &count) != 0 || count < 1) {
		bus_release_resource(sc->dev, SYS_RES_MEMORY,
		    sc->msix_table_rid, sc->msix_table);
		sc->msix_table = NULL;
		return (ENXIO);
	}

	sc->irq_rid = 1;	/* MSI-X vector 0 is resource id 1 */
	sc->irq = bus_alloc_resource_any(sc->dev, SYS_RES_IRQ, &sc->irq_rid,
	    RF_ACTIVE);
	if (sc->irq == NULL) {
		pci_release_msi(sc->dev);
		bus_release_resource(sc->dev, SYS_RES_MEMORY,
		    sc->msix_table_rid, sc->msix_table);
		sc->msix_table = NULL;
		return (ENXIO);
	}
	if (bus_setup_intr(sc->dev, sc->irq, INTR_TYPE_MISC | INTR_MPSAFE,
	    NULL, amd_intr, sc, &sc->intr_cookie) != 0) {
		bus_release_resource(sc->dev, SYS_RES_IRQ, sc->irq_rid, sc->irq);
		sc->irq = NULL;
		pci_release_msi(sc->dev);
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
		pci_release_msi(sc->dev);
	}
	if (sc->msix_table != NULL) {
		bus_release_resource(sc->dev, SYS_RES_MEMORY, sc->msix_table_rid,
		    sc->msix_table);
		sc->msix_table = NULL;
	}
	sc->msix_enabled = 0;
}
