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
	int eops = 0;

	wptr = amd_mmio_read32(sc, regIH_WPTR);
	while (sc->ih_rptr != wptr) {
		idx = sc->ih_rptr % ATRIUM_AMD_IH_ENTRIES;
		cookie = (const uint32_t *)((const char *)sc->ih_kva +
		    idx * ATRIUM_AMD_IH_COOKIE);
		if (cookie[0] == ATRIUM_AMD_IH_CAUSE_EOP)
			eops++;		/* each end-of-pipe cookie = one completion */
		/*
		 * ATRIUM_AMD_IH_CAUSE_VBLANK (DCN vertical blank, display-armed) is
		 * recognized and acknowledged by draining it here; it retires no
		 * syncobj. Its arrival is observable via irq_count (GET_IRQS) — the
		 * per-vblank kqueue knote on /dev/atrium-display0 is a later milestone.
		 */
		sc->ih_rptr++;
	}
	atomic_add_int(&sc->irq_count, 1);

	/*
	 * Retire completions: signal one pending syncobj per end-of-pipe event.
	 * This is the asynchronous half of submission — a submission whose ring
	 * parked on a cross-queue WAIT registered its syncobj here and is signalled
	 * now, on the *later* doorbell sweep that unblocked it (not inline at
	 * submit). amd_syncobj_signal takes so->lock nested under sc->lock; the
	 * syncobj's fo_close scrubs the list under sc->lock first, so the so is
	 * always live here. Also wakes any IOC_WAIT_FENCE poller.
	 */
	mtx_lock(&sc->lock);
	while (eops > 0 && sc->n_pending > 0) {
		struct atrium_amd_syncobj *so = sc->pending[0].so;
		uint64_t val = sc->pending[0].value;
		int i;

		for (i = 1; i < sc->n_pending; i++)
			sc->pending[i - 1] = sc->pending[i];
		sc->n_pending--;
		amd_syncobj_signal(so, val);
		eops--;
	}
	wakeup(&sc->irq_count);
	mtx_unlock(&sc->lock);
}

/*
 * Register a completion the ISR will hand to a syncobj. Called *before* the
 * doorbell rings, so a synchronous drain (whose IRQ fires inside the submit)
 * still finds the entry. Drops silently if the FIFO is full (bounded by
 * ATRIUM_AMD_MAX_PENDING in-flight signalled submissions).
 */
void
amd_pending_push(struct atrium_amd_softc *sc, struct atrium_amd_syncobj *so,
    uint64_t value)
{
	mtx_lock(&sc->lock);
	if (sc->n_pending < ATRIUM_AMD_MAX_PENDING) {
		sc->pending[sc->n_pending].so = so;
		sc->pending[sc->n_pending].value = value;
		sc->n_pending++;
	}
	mtx_unlock(&sc->lock);
}

/*
 * Remove every pending entry for a syncobj. The syncobj's fo_close calls this
 * under sc->lock before freeing, so the ISR (which holds sc->lock while it
 * signals) never touches a freed syncobj — no refcount, no free from the ISR.
 */
void
amd_pending_scrub(struct atrium_amd_softc *sc, struct atrium_amd_syncobj *so)
{
	int i, j;

	mtx_lock(&sc->lock);
	for (i = 0, j = 0; i < sc->n_pending; i++) {
		if (sc->pending[i].so != so) {
			if (j != i)
				sc->pending[j] = sc->pending[i];
			j++;
		}
	}
	sc->n_pending = j;
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
	/*
	 * The base (pci) module owns MSI-X: it allocated the table BAR and called
	 * pci_alloc_msix() as the device owner (§4.1). We just grab vector 0's IRQ
	 * resource and hook the ISR. If the base couldn't enable MSI-X, fall back
	 * to the synchronous-drain (poll) path.
	 */
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
	/* The MSI-X table BAR + pci_release_msi belong to the base (pci) module. */
	sc->msix_enabled = 0;
}
