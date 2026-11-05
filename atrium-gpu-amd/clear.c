/*
 * clear.c — the amd VRAM clear engine: how this GPU scrubs freed VRAM.
 *
 * The device allocator (vram.c) is arch-neutral and only asks "clear this
 * range, tell me when it is done". This is the amd answer, registered by the
 * gpu module: a CP DMA_DATA fill (SRC_SEL = DATA, value 0), run by the GPU on
 * a compute HQD and a GPUVM context that belong to the kernel alone. The CPU
 * never touches VRAM, which matters twice over: CPU writes through the BAR
 * are far too slow on gpusim (each is a socket round trip), and on a small-BAR
 * card most VRAM is not CPU-reachable at all.
 *
 *   queue  AMD_CLEAR_QID (HQD 2), doorbell page AMD_CLEAR_DOORBELL. Not in
 *          sc->hwq[]: no client can QUEUE_MAP, submit to or mmap it.
 *   VMID   AMD_CLEAR_VMID (0), the kernel context; vm.c hands out 1..15.
 *   VA     0x200000  one System page: the PM4 ring, and the fence at +2 KiB
 *          0x400000  a 64 MiB window of VRAM, re-pointed at each range
 *
 * One clear at a time (ce->lock, also held across a device reset). Each window
 * of the range is: write its PTEs, flush the TLB, program the queue, ring it,
 * and wait for the fence. The fills carry CP_SYNC, so the fence RELEASE_MEM
 * after them lands only once they are done. No interrupt is asked for (the
 * IH routes EOPs by client queue, and this one is not); the fence is polled,
 * at once (gpusim drains in the doorbell write) and then every millisecond.
 * A clear that does not finish in AMD_CLEAR_TIMEOUT_MS stops the queue and
 * fails, and vram.c keeps the range back and retries later.
 *
 * Real silicon: VMID 0 is the kernel GPUVM context there too; its page table
 * base is GCVM_CONTEXT0_PAGE_TABLE_BASE_ADDR, which the model's VM_CTX_*
 * registers stand for. An SDMA CONSTANT_FILL would do the same job off the CP;
 * it would be a second implementation of this file's one entry point.
 */
#include "atrium_gpu_amd.h"

#define AMD_CLEAR_RING_VA	0x200000ULL	/* PD 1, PTE 0 */
#define AMD_CLEAR_FENCE_OFF	2048		/* fence word in the ring page */
#define AMD_CLEAR_WIN_VA	0x400000ULL	/* PD 2 .. 2+WIN_PT-1 */
#define AMD_CLEAR_WIN_BYTES	((uint64_t)AMD_CLEAR_WIN_PT * 512 * PAGE_SIZE)
/* Bytes per DMA_DATA packet: a page multiple under the 26-bit byte count. */
#define AMD_CLEAR_CHUNK		(32ULL << 20)
#define AMD_CLEAR_TIMEOUT_MS	2000

CTASSERT(AMD_CLEAR_CHUNK <= DMA_DATA_MAX_BYTES);
/* Fills per window + the fence must fit the ring (RING_BYTES dwords). */
CTASSERT((AMD_CLEAR_WIN_BYTES / AMD_CLEAR_CHUNK + 1) * 7 <=
    ATRIUM_AMD_RING_BYTES);
CTASSERT(AMD_CLEAR_FENCE_OFF >= ATRIUM_AMD_RING_BYTES * 4);

static void
amd_clear_pages_free(struct amd_clear_engine *ce)
{
	int i;

	amd_dma_page_free(&ce->ring);
	amd_dma_page_free(&ce->pt_misc);
	for (i = 0; i < AMD_CLEAR_WIN_PT; i++)
		amd_dma_page_free(&ce->pt_win[i]);
	amd_dma_page_free(&ce->pdb);
}

/* Point VMID 0 at our page directory (every clear: a reset forgets it). */
static void
amd_clear_program_vm(struct atrium_amd_softc *sc)
{
	struct amd_clear_engine *ce = &sc->clear;

	mtx_lock(&sc->lock);
	amd_mmio_write32(sc, regVM_CTX_SELECT, AMD_CLEAR_VMID);
	amd_mmio_write32(sc, regVM_CTX_PT_BASE_LO,
	    (uint32_t)(ce->pdb.gpa & 0xffffffff));
	amd_mmio_write32(sc, regVM_CTX_PT_BASE_HI, (uint32_t)(ce->pdb.gpa >> 32));
	amd_mmio_write32(sc, regTLB_INVALIDATE, 1);
	mtx_unlock(&sc->lock);
}

/* Program the kernel HQD onto the ring under VMID 0 (resets its rptr). */
static void
amd_clear_program_queue(struct atrium_amd_softc *sc)
{
	mtx_lock(&sc->lock);
	amd_mmio_write32(sc, regHQD_SELECT, AMD_CLEAR_QID);
	amd_mmio_write32(sc, ATRIUM_AMD_Q_BASE + AMD_CLEAR_QID *
	    ATRIUM_AMD_Q_STRIDE + ATRIUM_AMD_QF_VMID, AMD_CLEAR_VMID);
	amd_mmio_write32(sc, regCP_HQD_PQ_BASE,
	    (uint32_t)(AMD_CLEAR_RING_VA >> 8));
	amd_mmio_write32(sc, regCP_HQD_PQ_CONTROL, ATRIUM_AMD_RING_BYTES);
	amd_mmio_write32(sc, regCP_HQD_PQ_DOORBELL_CONTROL, AMD_CLEAR_DOORBELL);
	amd_mmio_write32(sc, regCP_HQD_ACTIVE, 1);
	mtx_unlock(&sc->lock);
}

static void
amd_clear_stop_queue(struct atrium_amd_softc *sc)
{
	mtx_lock(&sc->lock);
	amd_mmio_write32(sc, regHQD_SELECT, AMD_CLEAR_QID);
	amd_mmio_write32(sc, regCP_HQD_ACTIVE, 0);
	mtx_unlock(&sc->lock);
}

/* Clear one window-sized piece: [off, off+len), len <= WIN_BYTES. */
static int
amd_clear_window(struct atrium_amd_softc *sc, uint64_t off, uint64_t len)
{
	struct amd_clear_engine *ce = &sc->clear;
	volatile uint64_t *fence;
	uint32_t *r;
	uint64_t done, chunk, va, i, npages = len / PAGE_SIZE;
	int n = 0, start, limit;

	sx_assert(&ce->lock, SA_XLOCKED);
	/* Point the window's PTEs at the range (writable VRAM). */
	for (i = 0; i < npages; i++) {
		uint64_t *pt = ce->pt_win[i / 512].kva;

		pt[i % 512] = ((off + i * PAGE_SIZE) & ~0xfffULL) |
		    ATRIUM_AMD_PTE_VALID | ATRIUM_AMD_PTE_VRAM |
		    ATRIUM_AMD_PTE_WRITE;
	}

	/* The ring: zero-fills, then the fence. */
	r = ce->ring.kva;
	for (done = 0; done < len; done += chunk) {
		chunk = MIN(len - done, AMD_CLEAR_CHUNK);
		va = AMD_CLEAR_WIN_VA + done;
		r[n++] = amd_pm4_type3_header(IT_DMA_DATA, 6);
		r[n++] = DMA_DATA_SRC_SEL_DATA | DMA_DATA_CP_SYNC;
		r[n++] = 0;			/* the fill value */
		r[n++] = 0;
		r[n++] = (uint32_t)(va & 0xffffffff);
		r[n++] = (uint32_t)(va >> 32);
		r[n++] = (uint32_t)chunk;	/* byte count */
	}
	ce->seq++;
	va = AMD_CLEAR_RING_VA + AMD_CLEAR_FENCE_OFF;
	r[n++] = amd_pm4_type3_header(IT_RELEASE_MEM, 6);
	r[n++] = 5u << 8;			/* end-of-pipe event */
	r[n++] = 2u << 29;			/* 64-bit data, no interrupt */
	r[n++] = (uint32_t)(va & 0xffffffff);
	r[n++] = (uint32_t)(va >> 32);
	r[n++] = (uint32_t)(ce->seq & 0xffffffff);
	r[n++] = (uint32_t)(ce->seq >> 32);
	fence = (volatile uint64_t *)((char *)ce->ring.kva +
	    AMD_CLEAR_FENCE_OFF);
	wmb();		/* PTEs and ring are in memory before the doorbell */

	amd_clear_program_vm(sc);	/* also flushes the TLB for the PTEs */
	amd_clear_program_queue(sc);
	bus_write_4(sc->doorbell, AMD_CLEAR_DOORBELL, n);

	start = ticks;
	limit = MAX(1, (AMD_CLEAR_TIMEOUT_MS * hz) / 1000);
	while (*fence != ce->seq) {
		if (ticks - start >= limit) {
			amd_clear_stop_queue(sc);
			device_printf(sc->dev, "VRAM clear of 0x%jx+%ju KiB "
			    "timed out\n", (uintmax_t)off,
			    (uintmax_t)(len >> 10));
			return (ETIMEDOUT);
		}
		pause_sbt("amdclr", SBT_1MS, 0, 0);
	}
	rmb();
	return (0);
}

/* The registered clear engine (amd_vram_clear_fn). */
static int
amd_clear_range(struct atrium_amd_softc *sc, uint64_t off, uint64_t len)
{
	struct amd_clear_engine *ce = &sc->clear;
	uint64_t n;
	int err = 0;

	if ((off & PAGE_MASK) != 0 || (len & PAGE_MASK) != 0)
		return (EINVAL);
	sx_xlock(&ce->lock);
	if (!ce->ready || sc->doorbell == NULL)
		err = ENXIO;
	while (err == 0 && len > 0) {
		n = MIN(len, AMD_CLEAR_WIN_BYTES);
		err = amd_clear_window(sc, off, n);
		off += n;
		len -= n;
	}
	sx_xunlock(&ce->lock);
	return (err);
}

/*
 * Build the kernel context's page tables and register with the allocator.
 * Called at gpu attach, after GMC/firmware/MES are up.
 */
int
amd_clear_init(struct atrium_amd_softc *sc)
{
	struct amd_clear_engine *ce = &sc->clear;
	uint64_t *pde, *pte;
	int err, i;

	bzero(ce, sizeof(*ce));
	sx_init(&ce->lock, "atrium_amd_clear");
	if ((err = amd_dma_page_alloc(sc, &ce->pdb)) != 0 ||
	    (err = amd_dma_page_alloc(sc, &ce->pt_misc)) != 0 ||
	    (err = amd_dma_page_alloc(sc, &ce->ring)) != 0)
		goto fail;
	for (i = 0; i < AMD_CLEAR_WIN_PT; i++)
		if ((err = amd_dma_page_alloc(sc, &ce->pt_win[i])) != 0)
			goto fail;

	pde = ce->pdb.kva;
	pde[(AMD_CLEAR_RING_VA >> ATRIUM_AMD_PD_SHIFT) & ATRIUM_AMD_PT_MASK] =
	    (ce->pt_misc.gpa & ~0xfffULL) | ATRIUM_AMD_PTE_VALID;
	for (i = 0; i < AMD_CLEAR_WIN_PT; i++)
		pde[((AMD_CLEAR_WIN_VA >> ATRIUM_AMD_PD_SHIFT) + i) &
		    ATRIUM_AMD_PT_MASK] =
		    (ce->pt_win[i].gpa & ~0xfffULL) | ATRIUM_AMD_PTE_VALID;
	/* The ring page: System memory, written by the fence. */
	pte = ce->pt_misc.kva;
	pte[(AMD_CLEAR_RING_VA >> ATRIUM_AMD_PT_SHIFT) & ATRIUM_AMD_PT_MASK] =
	    (ce->ring.gpa & ~0xfffULL) | ATRIUM_AMD_PTE_VALID |
	    ATRIUM_AMD_PTE_WRITE;
	ce->ready = 1;
	amd_vram_set_clear(sc, amd_clear_range);
	return (0);
fail:
	amd_clear_pages_free(ce);
	sx_destroy(&ce->lock);
	return (err);
}

/* gpu detach: unregister (waits out a running scrub), then free. */
void
amd_clear_fini(struct atrium_amd_softc *sc)
{
	struct amd_clear_engine *ce = &sc->clear;

	if (!ce->ready)
		return;
	amd_vram_set_clear(sc, NULL);
	sx_xlock(&ce->lock);
	ce->ready = 0;
	amd_clear_stop_queue(sc);
	mtx_lock(&sc->lock);
	/* Nothing may walk the tables we are about to free: point VMID 0 at
	 * the all-invalid directory (queue.c owns it; freed after us). */
	amd_mmio_write32(sc, regVM_CTX_SELECT, AMD_CLEAR_VMID);
	amd_mmio_write32(sc, regVM_CTX_PT_BASE_LO,
	    (uint32_t)(sc->null_pd.gpa & 0xffffffff));
	amd_mmio_write32(sc, regVM_CTX_PT_BASE_HI,
	    (uint32_t)(sc->null_pd.gpa >> 32));
	amd_mmio_write32(sc, regTLB_INVALIDATE, 1);
	mtx_unlock(&sc->lock);
	sx_xunlock(&ce->lock);
	amd_clear_pages_free(ce);
	sx_destroy(&ce->lock);
}

/* A device reset: no clear may run across the FLR (the scrub thread blocks
 * on the lock, then reprograms everything on its next window). Lock order:
 * ce->lock before the hwq locks; prepare takes it first, restore drops it
 * last. */
void
amd_clear_reset_prepare(struct atrium_amd_softc *sc)
{
	if (sc->clear.ready)
		sx_xlock(&sc->clear.lock);
}

void
amd_clear_reset_restore(struct atrium_amd_softc *sc)
{
	if (sc->clear.ready)
		sx_xunlock(&sc->clear.lock);
}
