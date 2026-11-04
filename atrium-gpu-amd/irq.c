/*
 * irq.c — the GPU module's IH cause handler for end-of-pipe completions, plus
 * the per-queue pending-completion lists it drains.
 *
 * The device-global IH ring + ISR live in the BASE module (ih.c): real silicon
 * carries GFX end-of-pipe, SDMA, and DCN vblank on ONE ring to ONE ISR. The base
 * drains that ring and routes each cookie by cause to whoever registered. This
 * module registers amd_eop_handler for IH_CAUSE_EOP (in atrium_amd_attach) and
 * owns the fence-retire half of submission — it has NO role in vblank, which is
 * a display signal the display module handles directly.
 *
 * Completions are tracked per hardware queue (review §A2.2). The EOP cookie
 * carries the queue it came from (ring_id), and the ISR tallies EOPs per queue
 * into sc->ih_eop_ring[] before calling us, so an end-of-pipe on one queue can
 * only retire that queue's oldest owed completion — never another queue's, and
 * so never another VM's that happens to be queued elsewhere.
 */
#include "atrium_gpu_amd.h"

static void
amd_pending_pop_locked(struct atrium_amd_qpend *qp,
    struct atrium_amd_pending *out)
{
	*out = qp->e[qp->head];
	qp->head = (qp->head + 1) % ATRIUM_AMD_MAX_PENDING;
	qp->n--;
}

/*
 * EOP cause handler — called by the base ISR with sc->lock HELD; `count` is the
 * total EOPs this pass and sc->ih_eop_ring[q] how many came from queue q. Each
 * one retires the head of its queue's list. A ring that parked on a
 * cross-queue WAIT is retired on the later doorbell sweep that unblocked it.
 * amd_syncobj_signal takes so->lock nested under sc->lock; a syncobj's fo_close
 * scrubs the lists under sc->lock first, so the so is always live here. Also
 * wakes any IOC_WAIT_FENCE poller.
 */
void
amd_eop_handler(struct atrium_amd_softc *sc, int count __unused)
{
	struct atrium_amd_pending p;
	int q;
	uint32_t n;

	mtx_assert(&sc->lock, MA_OWNED);
	for (q = 0; q < ATRIUM_AMD_NHWQ; q++) {
		struct atrium_amd_qpend *qp = &sc->qpend[q];

		n = sc->ih_eop_ring[q];
		if (n == 0)
			continue;
		qp->progress = ticks;
		while (n > 0 && qp->n > 0) {
			amd_pending_pop_locked(qp, &p);
			if (p.so != NULL)
				amd_syncobj_signal(p.so, p.value);
			n--;
		}
	}
	wakeup(&sc->irq_count);
}

/*
 * Register a completion queue `qid` owes. The caller holds sc->lock and rings
 * the doorbell after this, so a synchronous drain (whose IRQ fires inside the
 * doorbell write) still finds the entry. EBUSY if the queue already owes
 * ATRIUM_AMD_MAX_PENDING completions; the caller must not ring.
 */
int
amd_pending_push_locked(struct atrium_amd_softc *sc, uint32_t qid,
    struct atrium_amd_syncobj *so, uint64_t value)
{
	struct atrium_amd_qpend *qp = &sc->qpend[qid];
	int slot;

	mtx_assert(&sc->lock, MA_OWNED);
	if (qp->n >= ATRIUM_AMD_MAX_PENDING)
		return (EBUSY);
	slot = (qp->head + qp->n) % ATRIUM_AMD_MAX_PENDING;
	qp->e[slot].so = so;
	qp->e[slot].value = value;
	qp->n++;
	return (0);
}

/*
 * The work behind every completion queue `qid` owes is gone (the queue was
 * stopped, hung, or the GPU reset): fail each one so its waiters see `error`
 * instead of sleeping forever or being woken by someone else's EOP. Returns
 * how many were failed.
 */
int
amd_pending_fail_locked(struct atrium_amd_softc *sc, uint32_t qid, int error)
{
	struct atrium_amd_qpend *qp = &sc->qpend[qid];
	struct atrium_amd_pending p;
	int n = 0;

	mtx_assert(&sc->lock, MA_OWNED);
	while (qp->n > 0) {
		amd_pending_pop_locked(qp, &p);
		if (p.so != NULL) {
			amd_syncobj_fail(p.so, error);
			n++;
		}
	}
	qp->head = 0;
	sc->iso.fences_failed += n;
	return (n);
}

/*
 * Remove every pending entry for a syncobj. The syncobj's fo_close calls this
 * before freeing; it takes sc->lock, so the ISR (which holds sc->lock while it
 * signals) never touches a freed syncobj — no refcount, no free from the ISR.
 * The slot stays in the list as a NULL placeholder so the order of the
 * queue's remaining EOPs is unchanged: a scrubbed entry still consumes the EOP
 * its submit will raise.
 */
void
amd_pending_scrub(struct atrium_amd_softc *sc, struct atrium_amd_syncobj *so)
{
	int q, i;

	mtx_lock(&sc->lock);
	for (q = 0; q < ATRIUM_AMD_NHWQ; q++) {
		struct atrium_amd_qpend *qp = &sc->qpend[q];

		for (i = 0; i < qp->n; i++) {
			struct atrium_amd_pending *e =
			    &qp->e[(qp->head + i) % ATRIUM_AMD_MAX_PENDING];

			if (e->so == so)
				e->so = NULL;
		}
	}
	mtx_unlock(&sc->lock);
}
