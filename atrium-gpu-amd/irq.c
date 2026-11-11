/*
 * irq.c — the GPU module's IH cause handlers for end-of-pipe completions and
 * VM faults. The per-queue pending-completion lists they drain are Rust
 * (rs/pending.rs); these are the C entry points the base ISR calls.
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
#include "rs/glue.h"

/*
 * EOP cause handler — called by the base ISR with sc->lock HELD; `count` is the
 * total EOPs this pass and sc->ih_eop_ring[q] how many came from queue q. Each
 * one retires the head of its queue's list (amd_rs_eop). Also wakes any
 * IOC_WAIT_FENCE poller.
 */
void
amd_eop_handler(struct atrium_amd_softc *sc, int count __unused)
{
	mtx_assert(&sc->lock, MA_OWNED);
	amd_rs_eop(sc, sc->ih_eop_ring, sc->ih_fault_ring);
	wakeup(&sc->irq_count);
}

/*
 * VM fault cause handler (sc->lock held). Marks the queue's current work as
 * faulted, so its completions fail with EIO (here or in the EOP handler), and
 * asks the watchdog to stop the queue now if it is still running: a fault
 * costs that VM its work at once rather than after hang_ms. gpusim reports
 * faults on ring fetch, packet DMA and shader loads and stores (013b62b).
 */
void
amd_fault_handler(struct atrium_amd_softc *sc, int count)
{
	static struct timeval last;
	static int pps;
	int q, any = 0;

	mtx_assert(&sc->lock, MA_OWNED);
	sc->iso.vm_faults += count;
	for (q = 0; q < ATRIUM_AMD_NHWQ; q++) {
		if (sc->ih_fault_ring[q] == 0)
			continue;
		amd_rs_q_faulted_locked(sc, q);
		any = 1;
		if (ppsratecheck(&last, &pps, 5))
			device_printf(sc->dev, "VM fault: queue %d VMID %u "
			    "GPU-VA 0x%llx (%u this pass)\n", q,
			    sc->ih_fault_vmid[q],
			    (unsigned long long)sc->ih_fault_page[q] << 12,
			    sc->ih_fault_ring[q]);
	}
	if (any)
		amd_hwq_kick_watchdog(sc);
}
