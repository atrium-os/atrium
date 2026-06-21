/*
 * irq.c — the GPU module's IH cause handler for end-of-pipe completions, plus
 * the pending-completion FIFO it drains.
 *
 * The device-global IH ring + ISR live in the BASE module (ih.c): real silicon
 * carries GFX end-of-pipe, SDMA, and DCN vblank on ONE ring to ONE ISR. The base
 * drains that ring and routes each cookie by cause to whoever registered. This
 * module registers amd_eop_handler for IH_CAUSE_EOP (in atrium_amd_attach) and
 * owns the fence-retire half of submission — it has NO role in vblank, which is
 * a display signal the display module handles directly.
 */
#include "atrium_gpu_amd.h"

/*
 * EOP cause handler — called by the base ISR with sc->lock HELD, `count` = how
 * many end-of-pipe cookies this interrupt drained. Retire that many completions:
 * signal one pending syncobj per event. This is the asynchronous half of
 * submission — a submission whose ring parked on a cross-queue WAIT registered
 * its syncobj here and is signalled now, on the *later* doorbell sweep that
 * unblocked it (not inline at submit). amd_syncobj_signal takes so->lock nested
 * under sc->lock; the syncobj's fo_close scrubs the list under sc->lock first,
 * so the so is always live here. Also wakes any IOC_WAIT_FENCE poller.
 */
void
amd_eop_handler(struct atrium_amd_softc *sc, int count)
{
	while (count > 0 && sc->n_pending > 0) {
		struct atrium_amd_syncobj *so = sc->pending[0].so;
		uint64_t val = sc->pending[0].value;
		int i;

		for (i = 1; i < sc->n_pending; i++)
			sc->pending[i - 1] = sc->pending[i];
		sc->n_pending--;
		amd_syncobj_signal(so, val);
		count--;
	}
	wakeup(&sc->irq_count);
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
