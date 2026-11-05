/*
 * vram.c — the device VRAM allocator, owned by the BASE (pci) module.
 *
 * Device-local VRAM is a DEVICE resource, not a GPU-block one: the gpu's VRAM
 * buffer objects carve from it today, and a display cursor/overlay plane would
 * carve from the same pool tomorrow. So it lives in the shared softc
 * (struct atrium_amd_vram) and the carve happens here, in the device owner.
 *
 * Free space is a vmem(9) arena (best fit, coalescing). Freed VRAM does not go
 * straight back to it: it may still hold the last owner's pixels, so it is
 * DIRTY until the clear engine has scrubbed it, and only then does it rejoin
 * the arena. No client can ever be handed VRAM another client wrote. The
 * clear engine is per-architecture (amd: clear.c, a CP DMA fill on a kernel
 * queue) and is registered by the gpu module; this file knows only "clear this
 * range and tell me when it is done". With no engine registered, freed VRAM
 * stays dirty (lost until one is) rather than ever going out unscrubbed.
 *
 * Scrubbing runs on this allocator's own task thread, since a clear sleeps
 * while the GPU works. An allocation that finds too little clean VRAM while a
 * scrub is pending waits for it (bounded) before failing with ENOMEM.
 *
 * Sizes (small BAR): `total` is the device's VRAM, read from CONFIG_MEMSIZE;
 * `visible` is how much of it the CPU reaches through BAR0. They differ on a
 * discrete card without resizable BAR (gpusim: 8 GiB behind 256 MiB). The
 * kernel never touches VRAM with the CPU (the scrub is a GPU fill and VRAM BOs
 * are GPU-only), so ordinary allocations are placed above the window and keep
 * it for the ones that must be CPU-visible (AMD_VRAM_CPU_VISIBLE).
 *
 * A device FLR wipes VRAM but not this bookkeeping: live BOs keep their
 * offsets (their contents are lost, as ABI v2 says), and dirty ranges are
 * still scrubbed after the reset, since real silicon need not wipe.
 */
#include "atrium_gpu_amd.h"

static MALLOC_DEFINE(M_AMD_VRAM, "atrium_vram", "atrium GPU VRAM allocator");

#define AMD_VRAM_WAIT_MS	5000	/* an allocation waits this long for a scrub */
#define AMD_VRAM_RETRY_TICKS	(hz)	/* retry a failed scrub after ~1 s */

/* A sane CONFIG_MEMSIZE: some MiB, under 1 TiB; else the fallback. */
static uint64_t
amd_vram_probe_total(struct atrium_amd_softc *sc)
{
	uint32_t mb;

	if (sc->regs == NULL)
		return (ATRIUM_AMD_VRAM_BYTES);
	mb = amd_mmio_read32(sc, regCONFIG_MEMSIZE);
	if (mb == 0 || mb == 0xffffffff || mb > (1u << 20))
		return (ATRIUM_AMD_VRAM_BYTES);
	return ((uint64_t)mb << 20);
}

/* The size of BAR0, the CPU's window onto VRAM. Sized, not mapped (nothing in
 * the kernel needs a CPU mapping of VRAM). 0 if the device has no BAR0. */
static uint64_t
amd_vram_probe_bar(struct atrium_amd_softc *sc)
{
	struct resource *r;
	uint64_t size;
	int rid = ATRIUM_AMD_VRAM_BAR;

	r = bus_alloc_resource_any(sc->dev, SYS_RES_MEMORY, &rid, 0);
	if (r == NULL)
		return (0);
	size = rman_get_size(r);
	bus_release_resource(sc->dev, SYS_RES_MEMORY, rid, r);
	return (size);
}

/* --- scrubbing --------------------------------------------------------- */

/*
 * One scrub pass: take every dirty range, clear each with the engine and give
 * it back to the arena. A range the engine could not clear goes back on the
 * dirty list and the pass is retried later.
 */
static void
amd_vram_scrub_task(void *arg, int pending __unused)
{
	struct atrium_amd_softc *sc = arg;
	struct atrium_amd_vram *v = &sc->vram;
	TAILQ_HEAD(, amd_vram_dirty) work = TAILQ_HEAD_INITIALIZER(work);
	struct amd_vram_dirty *d;
	amd_vram_clear_fn *clear;
	int err, failed = 0;

	mtx_lock(&v->lock);
	clear = v->clear;
	if (clear == NULL || TAILQ_EMPTY(&v->dirty)) {
		v->passes++;
		wakeup(&v->passes);
		mtx_unlock(&v->lock);
		return;
	}
	TAILQ_CONCAT(&work, &v->dirty, link);
	v->scrubbing = 1;
	mtx_unlock(&v->lock);

	while ((d = TAILQ_FIRST(&work)) != NULL) {
		TAILQ_REMOVE(&work, d, link);
		err = clear(sc, d->off, d->size);
		mtx_lock(&v->lock);
		v->st.scrub_jobs++;
		if (err != 0) {
			uint64_t nfail = ++v->st.scrub_failed;
			uint64_t size = d->size, off = d->off;

			/* The engine is unwell (reset, hang): keep the range
			 * and the rest of this pass back, and retry later. */
			TAILQ_INSERT_TAIL(&v->dirty, d, link);
			TAILQ_CONCAT(&v->dirty, &work, link);
			mtx_unlock(&v->lock);
			/* Log the 1st, 2nd, 4th, 8th... failure. */
			if ((nfail & (nfail - 1)) == 0)
				device_printf(sc->dev, "VRAM scrub of %ju KiB at "
				    "0x%jx failed (%d): kept back\n",
				    (uintmax_t)(size >> 10), (uintmax_t)off, err);
			failed = 1;
			break;
		}
		vmem_xfree(v->arena, AMD_VRAM_ARENA_BASE + d->off, d->size);
		v->dirty_bytes -= d->size;
		v->st.scrubbed += d->size;
		mtx_unlock(&v->lock);
		free(d, M_AMD_VRAM);
	}

	mtx_lock(&v->lock);
	v->scrubbing = 0;
	v->passes++;
	wakeup(&v->passes);
	/* More was freed meanwhile, or this pass failed: go again. */
	if (!TAILQ_EMPTY(&v->dirty) && v->clear != NULL)
		taskqueue_enqueue_timeout(v->tq, &v->scrub_task,
		    failed ? AMD_VRAM_RETRY_TICKS : 0);
	mtx_unlock(&v->lock);
}

/* --- the allocator ----------------------------------------------------- */

static int
amd_vram_xalloc(struct atrium_amd_vram *v, uint64_t size, int flags,
    vmem_addr_t *addr)
{
	const vmem_addr_t base = AMD_VRAM_ARENA_BASE;

	mtx_assert(&v->lock, MA_OWNED);
	if ((flags & AMD_VRAM_CPU_VISIBLE) != 0)
		return (vmem_xalloc(v->arena, size, PAGE_SIZE, 0, 0, base,
		    base + v->visible - 1, M_BESTFIT | M_NOWAIT, addr));
	/* Above the CPU window first, so it stays free for what needs it. */
	if (v->visible < v->total &&
	    vmem_xalloc(v->arena, size, PAGE_SIZE, 0, 0, base + v->visible,
	    VMEM_ADDR_MAX, M_BESTFIT | M_NOWAIT, addr) == 0)
		return (0);
	return (vmem_xalloc(v->arena, size, PAGE_SIZE, 0, 0, VMEM_ADDR_MIN,
	    VMEM_ADDR_MAX, M_BESTFIT | M_NOWAIT, addr));
}

int
amd_vram_alloc(struct atrium_amd_softc *sc, uint64_t size, int flags,
    uint64_t *out_off)
{
	struct atrium_amd_vram *v = &sc->vram;
	vmem_addr_t addr;
	int start = ticks, waited = 0;
	int limit = MAX(1, (AMD_VRAM_WAIT_MS * hz) / 1000);

	if (size == 0 || (size & PAGE_MASK) != 0 || !v->inited)
		return (EINVAL);
	mtx_lock(&v->lock);
	while (amd_vram_xalloc(v, size, flags, &addr) != 0) {
		/*
		 * Not enough clean VRAM. If freed VRAM is still being scrubbed
		 * (and an engine exists to scrub it), wait for the next pass;
		 * otherwise this is a real shortage.
		 */
		if (v->dirty_bytes == 0 || v->clear == NULL ||
		    ticks - start >= limit) {
			v->st.enomem++;
			mtx_unlock(&v->lock);
			return (ENOMEM);
		}
		if (!waited) {
			waited = 1;
			v->st.alloc_waits++;
		}
		if (!v->scrubbing)
			taskqueue_enqueue_timeout(v->tq, &v->scrub_task, 0);
		(void)msleep(&v->passes, &v->lock, 0, "vramscr", hz / 10);
	}
	v->used += size;
	v->st.allocs++;
	if (v->used > v->st.peak_used)
		v->st.peak_used = v->used;
	mtx_unlock(&v->lock);
	*out_off = addr - AMD_VRAM_ARENA_BASE;
	return (0);
}

/*
 * Give back one whole allocation. The caller guarantees no GPU work can still
 * reach it (bo.c frees VRAM only after its queues drained, isolation break 2).
 * It is scrubbed before it can be allocated again. Never sleeps.
 */
void
amd_vram_free(struct atrium_amd_softc *sc, uint64_t off, uint64_t size)
{
	struct atrium_amd_vram *v = &sc->vram;
	struct amd_vram_dirty *d;

	if (size == 0 || !v->inited)
		return;
	d = malloc(sizeof(*d), M_AMD_VRAM, M_WAITOK | M_ZERO);
	d->off = off;
	d->size = size;
	mtx_lock(&v->lock);
	KASSERT(v->used >= size, ("VRAM free of %ju with %ju used",
	    (uintmax_t)size, (uintmax_t)v->used));
	v->used -= size;
	v->dirty_bytes += size;
	v->st.frees++;
	TAILQ_INSERT_TAIL(&v->dirty, d, link);
	if (v->clear != NULL && !v->scrubbing)
		taskqueue_enqueue_timeout(v->tq, &v->scrub_task, 0);
	mtx_unlock(&v->lock);
}

/*
 * Register the clear engine (the gpu module at attach) or remove it (NULL, at
 * detach). Removing waits for a running scrub to finish, so the engine's code
 * is never running once this returns.
 */
void
amd_vram_set_clear(struct atrium_amd_softc *sc, amd_vram_clear_fn *fn)
{
	struct atrium_amd_vram *v = &sc->vram;

	if (!v->inited)
		return;
	mtx_lock(&v->lock);
	v->clear = fn;
	if (fn != NULL && !TAILQ_EMPTY(&v->dirty))
		taskqueue_enqueue_timeout(v->tq, &v->scrub_task, 0);
	mtx_unlock(&v->lock);
	if (fn == NULL) {
		while (taskqueue_cancel_timeout(v->tq, &v->scrub_task, NULL) != 0)
			taskqueue_drain_timeout(v->tq, &v->scrub_task);
		/* Wake allocators: with no engine they now fail at once. */
		mtx_lock(&v->lock);
		wakeup(&v->passes);
		mtx_unlock(&v->lock);
	}
}

/* --- sysctl ------------------------------------------------------------ */

static int
amd_vram_sysctl(SYSCTL_HANDLER_ARGS)
{
	struct atrium_amd_softc *sc = arg1;
	struct atrium_amd_vram *v = &sc->vram;
	uint64_t val;

	mtx_lock(&v->lock);
	switch (arg2) {
	case 0: val = v->total; break;
	case 1: val = v->visible; break;
	case 2: val = v->used; break;
	case 3: val = v->dirty_bytes; break;
	case 4: val = v->total - v->used - v->dirty_bytes; break;
	case 5: val = v->st.peak_used; break;
	case 6: val = v->st.allocs; break;
	case 7: val = v->st.frees; break;
	case 8: val = v->st.scrubbed; break;
	case 9: val = v->st.scrub_jobs; break;
	case 10: val = v->st.scrub_failed; break;
	case 11: val = v->st.alloc_waits; break;
	case 12: val = v->st.enomem; break;
	case 13: val = v->clear != NULL; break;
	default: val = 0; break;
	}
	mtx_unlock(&v->lock);
	return (sysctl_handle_64(oidp, &val, 0, req));
}

static void
amd_vram_sysctls(struct atrium_amd_softc *sc)
{
	static const struct {
		const char *name, *desc;
	} f[] = {
		{ "total", "device VRAM, bytes (CONFIG_MEMSIZE)" },
		{ "visible", "VRAM the CPU reaches through BAR0, bytes" },
		{ "used", "VRAM allocated now, bytes" },
		{ "dirty", "freed VRAM not yet scrubbed, bytes" },
		{ "free", "clean VRAM ready to allocate, bytes" },
		{ "peak_used", "most VRAM allocated at once, bytes" },
		{ "allocs", "allocations" },
		{ "frees", "frees" },
		{ "scrubbed", "bytes the clear engine scrubbed" },
		{ "scrub_jobs", "clear-engine calls" },
		{ "scrub_failed", "clear-engine calls that failed (range kept back)" },
		{ "alloc_waits", "allocations that waited for a scrub" },
		{ "enomem", "allocations refused for lack of VRAM" },
		{ "clear_engine", "1 = a clear engine is registered" },
	};
	struct sysctl_ctx_list *ctx = device_get_sysctl_ctx(sc->dev);
	struct sysctl_oid *tree;
	int i;

	tree = SYSCTL_ADD_NODE(ctx,
	    SYSCTL_CHILDREN(device_get_sysctl_tree(sc->dev)), OID_AUTO, "vram",
	    CTLFLAG_RD | CTLFLAG_MPSAFE, NULL, "device VRAM allocator (vram.c)");
	for (i = 0; i < (int)nitems(f); i++)
		SYSCTL_ADD_PROC(ctx, SYSCTL_CHILDREN(tree), OID_AUTO, f[i].name,
		    CTLTYPE_U64 | CTLFLAG_RD | CTLFLAG_MPSAFE, sc, i,
		    amd_vram_sysctl, "QU", f[i].desc);
}

/* --- lifetime ---------------------------------------------------------- */

/* Base attach, once the register BAR is mapped. */
int
amd_vram_init(struct atrium_amd_softc *sc)
{
	struct atrium_amd_vram *v = &sc->vram;
	uint64_t bar;

	bzero(v, sizeof(*v));
	mtx_init(&v->lock, "atrium_vram_lock", NULL, MTX_DEF);
	TAILQ_INIT(&v->dirty);
	v->total = amd_vram_probe_total(sc);
	bar = amd_vram_probe_bar(sc);
	v->visible = (bar == 0 || bar > v->total) ? v->total : bar;
	v->arena = vmem_create("atrium_vram", AMD_VRAM_ARENA_BASE, v->total,
	    PAGE_SIZE, 0, M_WAITOK);
	if (v->arena == NULL) {
		mtx_destroy(&v->lock);
		return (ENOMEM);
	}
	v->tq = taskqueue_create("atrium_vram", M_WAITOK,
	    taskqueue_thread_enqueue, &v->tq);
	taskqueue_start_threads(&v->tq, 1, PWAIT, "%s vram scrub",
	    device_get_nameunit(sc->dev));
	TIMEOUT_TASK_INIT(v->tq, &v->scrub_task, 0, amd_vram_scrub_task, sc);
	v->inited = 1;
	amd_vram_sysctls(sc);
	device_printf(sc->dev, "VRAM %ju MiB, %ju MiB CPU-visible%s\n",
	    (uintmax_t)(v->total >> 20), (uintmax_t)(v->visible >> 20),
	    v->visible < v->total ? " (small BAR)" : "");
	return (0);
}

/* Base detach: the gpu module (and with it every BO and the clear engine) is
 * gone. Dirty ranges go down with the arena; the next attach FLRs. */
void
amd_vram_fini(struct atrium_amd_softc *sc)
{
	struct atrium_amd_vram *v = &sc->vram;
	struct amd_vram_dirty *d;

	if (!v->inited)
		return;
	amd_vram_set_clear(sc, NULL);
	taskqueue_free(v->tq);
	while ((d = TAILQ_FIRST(&v->dirty)) != NULL) {
		TAILQ_REMOVE(&v->dirty, d, link);
		vmem_xfree(v->arena, AMD_VRAM_ARENA_BASE + d->off, d->size);
		free(d, M_AMD_VRAM);
	}
	vmem_destroy(v->arena);
	v->inited = 0;
	mtx_destroy(&v->lock);
}
