/*
 * queue.c — who may use the device's hardware queues, and when memory a queue
 * could still touch may be freed. GPU isolation, docs/spec/gpu-isolation.md;
 * review docs/reviews/2026-11-03-gpu-kernel-and-tier2-review.md §A3:
 *
 *   break 1  Every submit used to reprogram a shared queue's ring and VMID and
 *            ring its doorbell with no lock, and with no regard for work still
 *            queued under another VMID. Now each queue has a sleepable lock
 *            around program + doorbell, and a queue is only reprogrammed once
 *            its read pointer has caught up with what was rung (QF_RPTR). A
 *            submitter waits up to ATRIUM_AMD_QUEUE_WAIT_MS, then gets EBUSY.
 *
 *   break 2  close(bo_fd) freed pages the GPU could still be reading, and
 *            close(vm_fd) freed page tables and the VMID under a queue still
 *            running in it. Now a BO's PTEs are cleared first, and its pages
 *            are freed only when every queue that was busy under one of its
 *            VMs has drained or been stopped (a deferred-free list reaped by a
 *            timeout task). A VM's teardown stops every queue programmed under
 *            its VMID and points the VMID at an all-invalid page directory
 *            before its page tables or VMID are released.
 *
 *   break 3  Any opener could mmap any doorbell, and QUEUE_MAP took a queue
 *            from whoever had it. Now QUEUE_MAP makes the caller the queue's
 *            exclusive owner (EBUSY for everyone else) until it closes the
 *            device or the VM is torn down. Only the owner can mmap that
 *            queue's doorbell page, through a managed device-pager object per
 *            grant, and the kernel removes the page from every mapping when it
 *            releases the queue.
 *
 * Lock order: hwq->lock (sx), then a pager object's lock or sc->defer_lock,
 * then sc->lock. Nothing here sleeps holding sc->lock or defer_lock.
 */
#include "atrium_gpu_amd.h"
#include "atrium_gpu_amd_abi.h"

#include <sys/rwlock.h>

#include <vm/vm_param.h>
#include <vm/vm_object.h>
#include <vm/vm_page.h>
#include <vm/vm_pager.h>
#include <vm/vm_phys.h>

#define AMD_REAP_TICKS	MAX(1, hz / 100)	/* poll deferred frees every ~10 ms */

static uint32_t
amd_hwq_rptr(struct atrium_amd_softc *sc, struct atrium_amd_hwq *q)
{
	return (amd_mmio_read32(sc, ATRIUM_AMD_Q_BASE +
	    q->qid * ATRIUM_AMD_Q_STRIDE + ATRIUM_AMD_QF_RPTR));
}

/*
 * Could the queue still be executing (or about to execute) work? A queue a
 * client owns is always busy: userspace rings it directly, so the kernel does
 * not know its write pointer. A kernel-rung queue is busy while its read
 * pointer trails the last write pointer the kernel rang. Lock held.
 */
static bool
amd_hwq_busy(struct atrium_amd_softc *sc, struct atrium_amd_hwq *q)
{
	sx_assert(&q->lock, SA_LOCKED);
	if (q->owner != NULL)
		return (true);
	if (q->vmid == 0 || q->wptr == 0)
		return (false);
	return (amd_hwq_rptr(sc, q) < q->wptr);
}

static struct atrium_amd_hwq *
amd_hwq_for_engine(struct atrium_amd_softc *sc, uint32_t engine)
{
	switch (engine) {
	case ATRIUM_GPU_ENGINE_GFX:
		return (&sc->hwq[ATRIUM_AMD_GFX_QID]);
	case ATRIUM_GPU_ENGINE_COMPUTE:
		return (&sc->hwq[ATRIUM_AMD_COMPUTE_QID]);
	default:
		return (NULL);
	}
}

/* ---------------------------------------------------------------------------
 * Doorbell grants: a managed device pager per grant, so the kernel can take the
 * page back out of every process that mapped it.
 */

static int
amd_db_pg_ctor(void *handle, vm_ooffset_t size, vm_prot_t prot,
    vm_ooffset_t foff, struct ucred *cred, u_short *color)
{
	*color = 0;
	return (0);
}

static void
amd_db_pg_dtor(void *handle)
{
	struct atrium_amd_db_grant *g = handle;
	struct atrium_amd_softc *sc = g->sc;

	mtx_lock(&sc->lock);
	sc->iso.grants_live--;
	mtx_unlock(&sc->lock);
	free(g, M_DEVBUF);
}

static int
amd_db_pg_fault(vm_object_t object, vm_ooffset_t offset, int prot,
    vm_page_t *mres)
{
	struct atrium_amd_db_grant *g = object->handle;
	vm_pindex_t pidx;
	vm_page_t page;

	/*
	 * VM_PAGER_ERROR, not VM_PAGER_FAIL: FAIL means "no page here" and
	 * vm_fault would quietly zero-fill an anonymous page in its place. A
	 * revoked doorbell should fault (SIGBUS) so the client knows.
	 */
	VM_OBJECT_ASSERT_WLOCKED(object);
	if (g->revoked || offset != 0)
		return (VM_PAGER_ERROR);
	pidx = OFF_TO_IDX(offset);
	page = PHYS_TO_VM_PAGE(g->pa);
	if (page == NULL || (page->flags & PG_FICTITIOUS) == 0)
		return (VM_PAGER_ERROR);
	/* Only one grant holds the page at a time: a revoke removes it from the
	 * old object before a new grant exists. Refuse rather than steal. */
	if (page->object != NULL)
		return (VM_PAGER_ERROR);
	vm_page_busy_acquire(page, 0);
	vm_page_valid(page);
	if (*mres != NULL)
		vm_page_replace(page, object, pidx, *mres);
	else if (vm_page_insert(page, object, pidx) != 0) {
		vm_page_xunbusy(page);
		return (VM_PAGER_ERROR);
	}
	*mres = page;
	return (VM_PAGER_OK);
}

static struct cdev_pager_ops amd_db_pg_ops = {
	.cdev_pg_fault = amd_db_pg_fault,
	.cdev_pg_ctor = amd_db_pg_ctor,
	.cdev_pg_dtor = amd_db_pg_dtor,
};

/* Take the doorbell page back from every mapping of the queue's grant. */
static void
amd_hwq_revoke(struct atrium_amd_softc *sc, struct atrium_amd_hwq *q)
{
	vm_object_t obj = q->grant_obj;

	sx_assert(&q->lock, SA_XLOCKED);
	if (obj == NULL)
		return;
	VM_OBJECT_WLOCK(obj);
	q->grant->revoked = 1;
	VM_OBJECT_WUNLOCK(obj);
	cdev_mgtdev_pager_free_pages(obj);	/* pmap_remove_all + remove */
	q->grant = NULL;
	q->grant_obj = NULL;
	vm_object_deallocate(obj);	/* the dtor frees the grant at last ref */
	mtx_lock(&sc->lock);
	sc->iso.revokes++;
	mtx_unlock(&sc->lock);
}

/*
 * Stop a queue and forget its programming: halt it (dropping queued work),
 * revoke the owner's doorbell and release ownership. Lock held exclusive.
 */
static void
amd_hwq_dequeue(struct atrium_amd_softc *sc, struct atrium_amd_hwq *q)
{
	bool busy;

	sx_assert(&q->lock, SA_XLOCKED);
	busy = amd_hwq_busy(sc, q);
	amd_hwq_revoke(sc, q);
	if (q->vmid != 0)
		amd_queue_stop(sc, q->qid);
	q->owner = NULL;
	q->owner_vm = NULL;
	q->vmid = 0;
	q->wptr = 0;
	q->gen++;
	if (busy) {
		mtx_lock(&sc->lock);
		sc->iso.dequeues++;
		mtx_unlock(&sc->lock);
	}
}

/*
 * Wait for a kernel-rung queue to drain. Holding the queue's lock while we
 * sleep is the point: no one else may program or ring it meanwhile.
 */
static int
amd_hwq_wait_idle(struct atrium_amd_softc *sc, struct atrium_amd_hwq *q)
{
	int waited = 0, ms;

	sx_assert(&q->lock, SA_XLOCKED);
	for (ms = 0; amd_hwq_busy(sc, q); ms++) {
		if (ms >= ATRIUM_AMD_QUEUE_WAIT_MS)
			return (EBUSY);
		if (!waited) {
			waited = 1;
			mtx_lock(&sc->lock);
			sc->iso.queue_waits++;
			mtx_unlock(&sc->lock);
		}
		pause_sbt("amdqidl", SBT_1MS, 0, 0);
	}
	return (0);
}

static void
amd_hwq_count_refused(struct atrium_amd_softc *sc)
{
	mtx_lock(&sc->lock);
	sc->iso.queue_refused++;
	mtx_unlock(&sc->lock);
}

/* ---------------------------------------------------------------------------
 * Deferred BO frees.
 */

/* Which queues are busy under any of `vmids`, with their generations. */
uint32_t
amd_hwq_busy_snapshot(struct atrium_amd_softc *sc, const uint16_t *vmids,
    int nvmids, uint64_t *gen)
{
	uint32_t mask = 0;
	int i, k;

	if (!sc->hwq_inited)
		return (0);
	for (i = 0; i < ATRIUM_AMD_NHWQ; i++) {
		struct atrium_amd_hwq *q = &sc->hwq[i];

		sx_slock(&q->lock);
		gen[i] = q->gen;
		for (k = 0; k < nvmids; k++) {
			if (q->vmid != 0 && q->vmid == vmids[k] &&
			    amd_hwq_busy(sc, q)) {
				mask |= 1u << i;
				break;
			}
		}
		sx_sunlock(&q->lock);
	}
	return (mask);
}

void
amd_hwq_defer_bo(struct atrium_amd_softc *sc, struct atrium_amd_bo *bo,
    uint32_t busy_mask, const uint64_t *gen)
{
	struct atrium_amd_deferred *d;
	int i;

	d = malloc(sizeof(*d), M_DEVBUF, M_WAITOK | M_ZERO);
	d->bo = bo;
	d->busy_mask = busy_mask;
	for (i = 0; i < ATRIUM_AMD_NHWQ; i++)
		d->gen[i] = gen[i];
	mtx_lock(&sc->defer_lock);
	TAILQ_INSERT_TAIL(&sc->deferred, d, link);
	mtx_unlock(&sc->defer_lock);
	mtx_lock(&sc->lock);
	sc->iso.deferred++;
	sc->iso.deferred_now++;
	mtx_unlock(&sc->lock);
	taskqueue_enqueue_timeout(taskqueue_thread, &sc->defer_task,
	    AMD_REAP_TICKS);
}

/*
 * Free every deferred BO whose queues have moved on: each queue that was busy
 * at close time is now idle, or has been reprogrammed or stopped since (its
 * generation changed, which only happens once it drained or was dequeued).
 */
static void
amd_hwq_reap(struct atrium_amd_softc *sc)
{
	TAILQ_HEAD(, atrium_amd_deferred) done = TAILQ_HEAD_INITIALIZER(done);
	struct atrium_amd_deferred *d, *tmp;
	uint64_t gen[ATRIUM_AMD_NHWQ];
	bool busy[ATRIUM_AMD_NHWQ];
	int i, n = 0, left;

	for (i = 0; i < ATRIUM_AMD_NHWQ; i++) {
		sx_slock(&sc->hwq[i].lock);
		gen[i] = sc->hwq[i].gen;
		busy[i] = amd_hwq_busy(sc, &sc->hwq[i]);
		sx_sunlock(&sc->hwq[i].lock);
	}
	mtx_lock(&sc->defer_lock);
	TAILQ_FOREACH_SAFE(d, &sc->deferred, link, tmp) {
		bool retired = true;

		for (i = 0; i < ATRIUM_AMD_NHWQ; i++) {
			if ((d->busy_mask & (1u << i)) != 0 &&
			    gen[i] == d->gen[i] && busy[i]) {
				retired = false;
				break;
			}
		}
		if (retired) {
			TAILQ_REMOVE(&sc->deferred, d, link);
			TAILQ_INSERT_TAIL(&done, d, link);
		}
	}
	left = !TAILQ_EMPTY(&sc->deferred);
	mtx_unlock(&sc->defer_lock);

	TAILQ_FOREACH_SAFE(d, &done, link, tmp) {
		sc->backend->bo_free(d->bo);
		free(d->bo, M_DEVBUF);
		free(d, M_DEVBUF);
		n++;
	}
	if (n > 0) {
		mtx_lock(&sc->lock);
		sc->iso.reaped += n;
		sc->iso.deferred_now -= n;
		mtx_unlock(&sc->lock);
	}
	if (left)
		taskqueue_enqueue_timeout(taskqueue_thread, &sc->defer_task,
		    AMD_REAP_TICKS);
}

static void
amd_hwq_reap_task(void *arg, int pending __unused)
{
	amd_hwq_reap(arg);
}

/* ---------------------------------------------------------------------------
 * The entry points.
 */

int
amd_hwq_submit(struct atrium_amd_softc *sc, struct atrium_amd_client *client,
    struct atrium_amd_bo *ring, uint32_t n_dwords, uint32_t engine,
    struct atrium_amd_vm *vm, uint32_t deadline_ns)
{
	struct atrium_amd_hwq *q;
	uint32_t doorbell_off;
	uint64_t ring_va;
	int err;

	q = amd_hwq_for_engine(sc, engine);
	if (q == NULL)
		return (EINVAL);
	if (n_dwords == 0)
		return (EINVAL);
	ring_va = amd_bo_gpu_va(ring, vm);	/* the ring's VA in the submit VM */
	if (ring_va == 0)
		return (EINVAL);		/* ring not bound in this VM */

	sx_xlock(&q->lock);
	if (q->owner != NULL) {
		/* A user-mode queue: only its owner, in the owning VM, may submit
		 * through the kernel too (it can only lose its own queued work). */
		if (q->owner != client || q->owner_vm != vm) {
			sx_xunlock(&q->lock);
			amd_hwq_count_refused(sc);
			return (EBUSY);
		}
	} else if ((err = amd_hwq_wait_idle(sc, q)) != 0) {
		sx_xunlock(&q->lock);
		amd_hwq_count_refused(sc);
		return (err);
	}
	/* Stage the frame deadline, program, ring: one atomic sequence. */
	if (sc->regs != NULL)
		amd_mmio_write32(sc, regCP_SUBMIT_DEADLINE_NS, deadline_ns);
	err = amd_queue_program(sc, ring_va, engine, vm->vmid, &doorbell_off);
	if (err == 0) {
		q->vmid = vm->vmid;
		q->wptr = n_dwords;
		q->gen++;
		bus_write_4(sc->doorbell, doorbell_off, n_dwords);
	}
	sx_xunlock(&q->lock);
	return (err);
}

int
amd_hwq_map(struct atrium_amd_softc *sc, struct atrium_amd_client *client,
    struct atrium_amd_vm *vm, uint64_t ring_va, uint32_t engine,
    uint32_t *doorbell_off)
{
	struct atrium_amd_hwq *q;
	int err;

	q = amd_hwq_for_engine(sc, engine);
	if (q == NULL)
		return (EINVAL);
	sx_xlock(&q->lock);
	if (q->owner != NULL && (q->owner != client || q->owner_vm != vm)) {
		sx_xunlock(&q->lock);
		amd_hwq_count_refused(sc);
		return (EBUSY);
	}
	if (q->owner == NULL && (err = amd_hwq_wait_idle(sc, q)) != 0) {
		sx_xunlock(&q->lock);
		amd_hwq_count_refused(sc);
		return (err);
	}
	err = amd_queue_program(sc, ring_va, engine, vm->vmid, doorbell_off);
	if (err == 0) {
		q->owner = client;
		q->owner_vm = vm;
		q->vmid = vm->vmid;
		q->wptr = 0;		/* userspace rings it from here on */
		q->gen++;
	}
	sx_xunlock(&q->lock);
	return (err);
}

int
amd_hwq_mmap_single(struct atrium_amd_softc *sc,
    struct atrium_amd_client *client, vm_ooffset_t *offset, vm_size_t size,
    vm_object_t *object, int nprot)
{
	struct atrium_amd_db_grant *g;
	vm_object_t obj;
	int i;

	if (size != PAGE_SIZE || sc->doorbell == NULL || !sc->db_fict_registered)
		return (EINVAL);
	for (i = 0; i < ATRIUM_AMD_NHWQ; i++) {
		struct atrium_amd_hwq *q = &sc->hwq[i];

		sx_xlock(&q->lock);
		if (q->owner != client || q->doorbell_off != *offset) {
			sx_xunlock(&q->lock);
			continue;
		}
		if (q->grant_obj == NULL) {
			g = malloc(sizeof(*g), M_DEVBUF, M_WAITOK | M_ZERO);
			g->sc = sc;
			g->pa = rman_get_start(sc->doorbell) + q->doorbell_off;
			mtx_lock(&sc->lock);
			sc->iso.grants_live++;	/* the dtor undoes it */
			sc->iso.grants++;
			mtx_unlock(&sc->lock);
			obj = cdev_pager_allocate(g, OBJT_MGTDEVICE,
			    &amd_db_pg_ops, PAGE_SIZE, nprot, 0,
			    curthread->td_ucred);
			if (obj == NULL) {
				/* The ctor never ran, so neither will the dtor. */
				mtx_lock(&sc->lock);
				sc->iso.grants_live--;
				mtx_unlock(&sc->lock);
				free(g, M_DEVBUF);
				sx_xunlock(&q->lock);
				return (ENOMEM);
			}
			q->grant = g;
			q->grant_obj = obj;
		}
		vm_object_reference(q->grant_obj);
		*object = q->grant_obj;
		*offset = 0;
		sx_xunlock(&q->lock);
		return (0);
	}
	/* Not a doorbell of a queue this client owns. */
	return (EACCES);
}

/* The client closed its last fd on the device: give back its queues. */
void
amd_hwq_client_close(struct atrium_amd_softc *sc,
    struct atrium_amd_client *client)
{
	int i;

	for (i = 0; i < ATRIUM_AMD_NHWQ; i++) {
		struct atrium_amd_hwq *q = &sc->hwq[i];

		sx_xlock(&q->lock);
		if (q->owner == client)
			amd_hwq_dequeue(sc, q);
		sx_xunlock(&q->lock);
	}
	amd_hwq_reap(sc);
}

/*
 * A VM is going away: stop every queue programmed under its VMID (whatever is
 * still queued there belongs to a VM that no longer exists), then point the
 * VMID at the all-invalid page directory so nothing can walk the page tables
 * the caller is about to free. Only then may the VMID be handed out again.
 */
void
amd_hwq_vm_gone(struct atrium_amd_softc *sc, uint16_t vmid)
{
	int i;

	if (!sc->hwq_inited || vmid == 0)
		return;
	for (i = 0; i < ATRIUM_AMD_NHWQ; i++) {
		struct atrium_amd_hwq *q = &sc->hwq[i];

		sx_xlock(&q->lock);
		if (q->vmid == vmid)
			amd_hwq_dequeue(sc, q);
		sx_xunlock(&q->lock);
	}
	mtx_lock(&sc->lock);
	amd_mmio_write32(sc, regVM_CTX_SELECT, vmid);
	amd_mmio_write32(sc, regVM_CTX_PT_BASE_LO,
	    (uint32_t)(sc->null_pd.gpa & 0xffffffff));
	amd_mmio_write32(sc, regVM_CTX_PT_BASE_HI,
	    (uint32_t)(sc->null_pd.gpa >> 32));
	amd_mmio_write32(sc, regTLB_INVALIDATE, 1);
	mtx_unlock(&sc->lock);
	amd_hwq_reap(sc);
}

/*
 * Device reset: hold every queue across the FLR (no submit, map or mmap can
 * start), and drop all queue state, since the FLR drops the rings themselves.
 * Owners lose their doorbell mappings and must QUEUE_MAP again.
 */
void
amd_hwq_reset_prepare(struct atrium_amd_softc *sc)
{
	int i;

	for (i = 0; i < ATRIUM_AMD_NHWQ; i++) {
		sx_xlock(&sc->hwq[i].lock);
		amd_hwq_dequeue(sc, &sc->hwq[i]);
	}
}

void
amd_hwq_reset_restore(struct atrium_amd_softc *sc)
{
	int i;

	for (i = ATRIUM_AMD_NHWQ - 1; i >= 0; i--)
		sx_xunlock(&sc->hwq[i].lock);
	amd_hwq_reap(sc);
}

/* Anything that keeps the module's code reachable after detach. */
int
amd_hwq_busy_refs(struct atrium_amd_softc *sc)
{
	int n;

	mtx_lock(&sc->lock);
	n = sc->iso.grants_live + sc->iso.deferred_now;
	mtx_unlock(&sc->lock);
	return (n);
}

static int
amd_iso_sysctl_u64(SYSCTL_HANDLER_ARGS)
{
	struct atrium_amd_softc *sc = arg1;
	uint64_t v;

	mtx_lock(&sc->lock);
	v = *(uint64_t *)((char *)&sc->iso + arg2);
	mtx_unlock(&sc->lock);
	return (sysctl_handle_64(oidp, &v, 0, req));
}

static int
amd_iso_sysctl_int(SYSCTL_HANDLER_ARGS)
{
	struct atrium_amd_softc *sc = arg1;
	int v;

	mtx_lock(&sc->lock);
	v = *(int *)((char *)&sc->iso + arg2);
	mtx_unlock(&sc->lock);
	return (sysctl_handle_int(oidp, &v, 0, req));
}

int
amd_hwq_init(struct atrium_amd_softc *sc, device_t dev)
{
	struct sysctl_ctx_list *ctx = device_get_sysctl_ctx(dev);
	struct sysctl_oid *tree;
	vm_paddr_t start;
	int err, i;

	bzero(&sc->iso, sizeof(sc->iso));
	for (i = 0; i < ATRIUM_AMD_NHWQ; i++) {
		struct atrium_amd_hwq *q = &sc->hwq[i];

		bzero(q, sizeof(*q));
		sx_init(&q->lock, "atrium_amd_hwq");
		q->qid = i;
		q->engine = (i == ATRIUM_AMD_GFX_QID) ?
		    ATRIUM_GPU_ENGINE_GFX : ATRIUM_GPU_ENGINE_COMPUTE;
		q->doorbell_off = (i == ATRIUM_AMD_GFX_QID) ?
		    ATRIUM_AMD_GFX_DOORBELL : ATRIUM_AMD_COMPUTE_DOORBELL;
	}
	mtx_init(&sc->defer_lock, "atrium_amd_defer", NULL, MTX_DEF);
	TAILQ_INIT(&sc->deferred);
	TIMEOUT_TASK_INIT(taskqueue_thread, &sc->defer_task, 0,
	    amd_hwq_reap_task, sc);
	sc->hwq_inited = 1;

	err = amd_dma_page_alloc(sc, &sc->null_pd);	/* zeroed: all invalid */
	if (err != 0)
		return (err);

	/* The doorbell BAR's pages as fictitious pages the grant pager inserts. */
	if (sc->doorbell != NULL) {
		start = rman_get_start(sc->doorbell);
		err = vm_phys_fictitious_reg_range(start,
		    start + round_page(rman_get_size(sc->doorbell)),
		    VM_MEMATTR_DEVICE);
		if (err != 0)
			return (err);
		sc->db_fict_registered = 1;
	}

	tree = SYSCTL_ADD_NODE(ctx, SYSCTL_CHILDREN(device_get_sysctl_tree(dev)),
	    OID_AUTO, "isolation", CTLFLAG_RD | CTLFLAG_MPSAFE, NULL,
	    "GPU isolation counters (gpu-isolation.md)");
#define AMD_ISO_U64(name, field, desc)					\
	SYSCTL_ADD_PROC(ctx, SYSCTL_CHILDREN(tree), OID_AUTO, name,	\
	    CTLTYPE_U64 | CTLFLAG_RD | CTLFLAG_MPSAFE, sc,		\
	    offsetof(struct atrium_amd_isolation_stats, field),		\
	    amd_iso_sysctl_u64, "QU", desc)
#define AMD_ISO_INT(name, field, desc)					\
	SYSCTL_ADD_PROC(ctx, SYSCTL_CHILDREN(tree), OID_AUTO, name,	\
	    CTLTYPE_INT | CTLFLAG_RD | CTLFLAG_MPSAFE, sc,		\
	    offsetof(struct atrium_amd_isolation_stats, field),		\
	    amd_iso_sysctl_int, "I", desc)
	AMD_ISO_U64("queue_waits", queue_waits,
	    "submits that waited for another VM's work to drain");
	AMD_ISO_U64("queue_refused", queue_refused,
	    "submits and maps refused with EBUSY");
	AMD_ISO_U64("dequeues", dequeues, "queues stopped with work dropped");
	AMD_ISO_U64("grants", grants, "doorbell grants created");
	AMD_ISO_U64("revokes", revokes, "doorbell grants revoked");
	AMD_ISO_U64("deferred", deferred, "BO frees deferred");
	AMD_ISO_U64("reaped", reaped, "deferred BO frees completed");
	AMD_ISO_INT("deferred_now", deferred_now, "deferred BO frees pending");
	AMD_ISO_INT("grants_live", grants_live, "doorbell grant objects alive");
#undef AMD_ISO_U64
#undef AMD_ISO_INT
	return (0);
}

/*
 * Detach (the caller has checked amd_hwq_busy_refs == 0 and no BO or VM is
 * open, so no queue has an owner or a grant left).
 */
void
amd_hwq_fini(struct atrium_amd_softc *sc)
{
	vm_paddr_t start;
	int i;

	if (!sc->hwq_inited)
		return;
	for (i = 0; i < ATRIUM_AMD_NHWQ; i++) {
		sx_xlock(&sc->hwq[i].lock);
		amd_hwq_dequeue(sc, &sc->hwq[i]);
		sx_xunlock(&sc->hwq[i].lock);
	}
	while (taskqueue_cancel_timeout(taskqueue_thread, &sc->defer_task,
	    NULL) != 0)
		taskqueue_drain_timeout(taskqueue_thread, &sc->defer_task);
	amd_hwq_reap(sc);
	taskqueue_drain_timeout(taskqueue_thread, &sc->defer_task);
	if (sc->db_fict_registered) {
		start = rman_get_start(sc->doorbell);
		vm_phys_fictitious_unreg_range(start,
		    start + round_page(rman_get_size(sc->doorbell)));
		sc->db_fict_registered = 0;
	}
	amd_dma_page_free(&sc->null_pd);
	for (i = 0; i < ATRIUM_AMD_NHWQ; i++)
		sx_destroy(&sc->hwq[i].lock);
	mtx_destroy(&sc->defer_lock);
	sc->hwq_inited = 0;
}
