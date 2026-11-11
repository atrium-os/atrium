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
 *   A2.2/A2.6  Completions are owed per queue (irq.c), and a watchdog stops
 *            a kernel-rung queue that shows no progress for hang_ms (default
 *            ATRIUM_AMD_HANG_MS): no read-pointer movement and no EOP. Stopping
 *            a queue, for any reason, fails what it owed with EIO, so a waiter
 *            learns its work is gone and the next VM gets the queue.
 *
 * Lock order: hwq->lock (sx), then a pager object's lock or the deferred
 * list's lock (rs/defer.rs), then sc->lock. Nothing sleeps holding sc->lock
 * or the deferred list's lock.
 */
#include "atrium_gpu_amd.h"
#include "atrium_gpu_amd_abi.h"
#include "rs/glue.h"

#include <sys/rwlock.h>

#include <vm/vm_param.h>
#include <vm/vm_object.h>
#include <vm/vm_page.h>
#include <vm/vm_pager.h>
#include <vm/vm_phys.h>

#define AMD_REAP_TICKS	MAX(1, hz / 100)	/* poll deferred frees every ~10 ms */
#define AMD_WATCHDOG_TICKS MAX(1, hz / 10)	/* look for hangs every ~100 ms */

static uint32_t
amd_hwq_rptr(struct atrium_amd_softc *sc, struct atrium_amd_hwq *q)
{
	return (amd_mmio_read32(sc, ATRIUM_AMD_Q_BASE +
	    q->qid * ATRIUM_AMD_Q_STRIDE + ATRIUM_AMD_QF_RPTR));
}

/* Does the queue still owe a completion (an EOP not yet seen)? */
static bool
amd_hwq_owes(struct atrium_amd_softc *sc, struct atrium_amd_hwq *q)
{
	bool owes;

	mtx_lock(&sc->lock);
	owes = sc->qpend[q->qid].n > 0;
	mtx_unlock(&sc->lock);
	return (owes);
}

/* Has the CP not yet fetched everything the kernel rang? Lock held. */
static bool
amd_hwq_fetching(struct atrium_amd_softc *sc, struct atrium_amd_hwq *q)
{
	sx_assert(&q->lock, SA_LOCKED);
	if (q->vmid == 0 || q->wptr == 0)
		return (false);
	return (amd_hwq_rptr(sc, q) < q->wptr);
}

/*
 * Could the queue still be executing (or about to execute) work? A queue a
 * client owns is always busy: userspace rings it directly, so the kernel does
 * not know its write pointer. A kernel-rung queue is busy while its read
 * pointer trails the last write pointer the kernel rang, or while it owes an
 * end-of-pipe: the CP can be past the last packet with shaders still running
 * (and gpusim's shaping mode defers the EOP on purpose). Lock held.
 */
static bool
amd_hwq_busy(struct atrium_amd_softc *sc, struct atrium_amd_hwq *q)
{
	sx_assert(&q->lock, SA_LOCKED);
	if (q->owner != NULL)
		return (true);
	return (amd_hwq_fetching(sc, q) || amd_hwq_owes(sc, q));
}

/*
 * May the queue be reprogrammed for `vmid` now? Never while the CP is still
 * fetching (reprogramming drops the rest of the ring). Work the same VM still
 * owes an EOP for may be followed by more of its own work, which retires in
 * order behind it; another VM must wait for the queue to be fully idle.
 */
static bool
amd_hwq_blocks(struct atrium_amd_softc *sc, struct atrium_amd_hwq *q,
    uint16_t vmid)
{
	if (amd_hwq_fetching(sc, q))
		return (true);
	return (q->vmid != vmid && amd_hwq_owes(sc, q));
}

/* Run the watchdog now (from the fault handler: sc->lock may be held). */
void
amd_hwq_kick_watchdog(struct atrium_amd_softc *sc)
{
	if (!sc->hwq_inited)
		return;
	sc->watchdog_armed = 1;
	taskqueue_enqueue_timeout(taskqueue_thread, &sc->watchdog_task, 1);
}

/* Make sure the hang watchdog is running (there is kernel-rung work). */
static void
amd_hwq_arm_watchdog(struct atrium_amd_softc *sc)
{
	bool arm;

	mtx_lock(&sc->lock);
	arm = !sc->watchdog_armed;
	sc->watchdog_armed = 1;
	mtx_unlock(&sc->lock);
	if (arm)
		taskqueue_enqueue_timeout(taskqueue_thread, &sc->watchdog_task,
		    AMD_WATCHDOG_TICKS);
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
 * Doorbell grants: a managed device pager per grant (rs/doorbell.rs), so the
 * kernel can take the page back out of every process that mapped it.
 */

/* Take the doorbell page back from every mapping of the queue's grant. */
static void
amd_hwq_revoke(struct atrium_amd_softc *sc, struct atrium_amd_hwq *q)
{
	vm_object_t obj = q->grant_obj;

	sx_assert(&q->lock, SA_XLOCKED);
	if (obj == NULL)
		return;
	q->grant_obj = NULL;
	amd_db_grant_revoke(obj);	/* faults now fail; pages unmapped */
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
	mtx_lock(&sc->lock);
	/* The stopped work will raise no EOP: fail what it owed. */
	amd_pending_fail_locked(sc, q->qid, EIO);
	sc->qpend[q->qid].faulted = 0;
	if (busy)
		sc->iso.dequeues++;
	mtx_unlock(&sc->lock);
}

static int amd_hwq_hang_limit(struct atrium_amd_softc *sc);

/*
 * The hang check (review §A2.6): has this busy kernel-rung queue shown no
 * progress (read pointer movement or an EOP) for hang_ms? If so stop it,
 * which fails what it owed. Run by the watchdog and by a submitter waiting
 * for the queue, since the waiter holds the lock the watchdog would need.
 * Returns true if the queue was stopped. Lock held exclusive.
 */
static bool
amd_hwq_check_hang(struct atrium_amd_softc *sc, struct atrium_amd_hwq *q)
{
	struct atrium_amd_qpend *qp = &sc->qpend[q->qid];
	int limit = amd_hwq_hang_limit(sc);
	bool hung = false;
	uint32_t rptr;

	sx_assert(&q->lock, SA_XLOCKED);
	if (q->owner != NULL || !amd_hwq_busy(sc, q))
		return (false);
	rptr = q->vmid != 0 ? amd_hwq_rptr(sc, q) : 0;
	mtx_lock(&sc->lock);
	if (qp->faulted) {
		/* A VM fault: its work is already wrong; stop it now. */
		mtx_unlock(&sc->lock);
		device_printf(sc->dev, "queue %u (VMID %u) faulted: stopped\n",
		    q->qid, q->vmid);
		amd_hwq_dequeue(sc, q);
		return (true);
	}
	if (rptr != qp->last_rptr) {
		qp->last_rptr = rptr;
		qp->progress = ticks;
	} else if (ticks - qp->progress >= limit) {
		hung = true;
		sc->iso.hangs++;
	}
	mtx_unlock(&sc->lock);
	if (hung) {
		device_printf(sc->dev, "queue %u (VMID %u) made no progress "
		    "for %d ms: stopped\n", q->qid, q->vmid, sc->hang_ms);
		amd_hwq_dequeue(sc, q);
	}
	return (hung);
}

/*
 * Wait for a kernel-rung queue to drain. Holding the queue's lock while we
 * sleep is the point: no one else may program or ring it meanwhile.
 */
static int
amd_hwq_wait_idle(struct atrium_amd_softc *sc, struct atrium_amd_hwq *q,
    uint16_t vmid)
{
	int waited = 0, n, start = ticks;
	int limit = MAX(1, (ATRIUM_AMD_QUEUE_WAIT_MS * hz) / 1000);

	/*
	 * Bounded by the clock, not by counting 1 ms pauses: a pause can run
	 * long on a loaded or emulated machine, and 2000 of them then outlast
	 * the hang limit, so the parked owner is stopped as hung instead of
	 * the waiter being refused.
	 */
	sx_assert(&q->lock, SA_XLOCKED);
	for (n = 0; amd_hwq_blocks(sc, q, vmid); n++) {
		if (n % 10 == 0 && amd_hwq_check_hang(sc, q))
			break;
		if (ticks - start >= limit)
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

/* Which queues are busy now, under any VM, with their generations. */
uint32_t
amd_hwq_busy_now(struct atrium_amd_softc *sc, uint64_t *gen)
{
	uint32_t mask = 0;
	int i;

	for (i = 0; i < ATRIUM_AMD_NHWQ; i++) {
		struct atrium_amd_hwq *q = &sc->hwq[i];

		sx_slock(&q->lock);
		gen[i] = q->gen;
		if (amd_hwq_busy(sc, q))
			mask |= 1u << i;
		sx_sunlock(&q->lock);
	}
	return (mask);
}

void
amd_hwq_defer_kick(struct atrium_amd_softc *sc)
{
	taskqueue_enqueue_timeout(taskqueue_thread, &sc->defer_task,
	    AMD_REAP_TICKS);
}

/*
 * The deferred-free list itself is the front-end's (rs/defer.rs): it frees
 * every BO whose queues have moved on. Run it, and again later while BOs wait.
 */
static void
amd_hwq_reap(struct atrium_amd_softc *sc)
{
	if (amd_rs_reap(sc))
		amd_hwq_defer_kick(sc);
}

static void
amd_hwq_reap_task(void *arg, int pending __unused)
{
	amd_hwq_reap(arg);
}

/* ---------------------------------------------------------------------------
 * The hang watchdog (review §A2.6).
 *
 * Runs every ~100 ms while any kernel-rung queue is busy. A queue makes
 * progress when its read pointer moves or it raises an EOP; one that has made
 * none for hang_ms is hung (a ring parked on a wait nobody satisfies, a
 * faulted ring, or work whose EOP never comes). It is stopped like any other
 * dequeue: what it owed fails with EIO and the next VM can have the queue.
 * A queue a client owns is left alone: userspace rings it, so the kernel
 * cannot tell idle from stuck, and the owner can only stall itself.
 */
static int
amd_hwq_hang_limit(struct atrium_amd_softc *sc)
{
	int ms;

	mtx_lock(&sc->lock);
	ms = sc->hang_ms;
	mtx_unlock(&sc->lock);
	return (MAX(1, (int)(((int64_t)ms * hz) / 1000)));
}

static void
amd_hwq_watchdog(struct atrium_amd_softc *sc)
{
	int i, active = 0;

	for (i = 0; i < ATRIUM_AMD_NHWQ; i++) {
		struct atrium_amd_hwq *q = &sc->hwq[i];

		/* A submitter holding the queue runs the same check while it
		 * waits (amd_hwq_wait_idle); look again next round. */
		if (!sx_try_xlock(&q->lock)) {
			active = 1;
			continue;
		}
		if (!amd_hwq_check_hang(sc, q) && q->owner == NULL &&
		    amd_hwq_busy(sc, q))
			active = 1;
		sx_xunlock(&q->lock);
	}
	mtx_lock(&sc->lock);
	sc->watchdog_armed = active;
	mtx_unlock(&sc->lock);
	if (active)
		taskqueue_enqueue_timeout(taskqueue_thread, &sc->watchdog_task,
		    AMD_WATCHDOG_TICKS);
	amd_hwq_reap(sc);
}

static void
amd_hwq_watchdog_task(void *arg, int pending __unused)
{
	amd_hwq_watchdog(arg);
}

/* ---------------------------------------------------------------------------
 * The entry points.
 */

int
amd_hwq_submit(struct atrium_amd_softc *sc, struct atrium_amd_client *client,
    uint64_t ring_va, uint32_t n_dwords, uint32_t engine,
    struct atrium_amd_vm *vm, uint32_t deadline_ns,
    struct atrium_amd_syncobj *so, uint64_t signal_value)
{
	struct atrium_amd_hwq *q;
	uint32_t doorbell_off;
	int err;

	/* The arguments were checked by the front-end (rs/ioctl.rs). */
	q = amd_hwq_for_engine(sc, engine);
	if (q == NULL)
		return (EINVAL);

	sx_xlock(&q->lock);
	if (q->owner != NULL) {
		/* A user-mode queue: only its owner, in the owning VM, may submit
		 * through the kernel too (it can only lose its own queued work). */
		if (q->owner != client || q->owner_vm != vm) {
			sx_xunlock(&q->lock);
			amd_hwq_count_refused(sc);
			return (EBUSY);
		}
	} else if ((err = amd_hwq_wait_idle(sc, q, vm->vmid)) != 0) {
		sx_xunlock(&q->lock);
		amd_hwq_count_refused(sc);
		return (err);
	}
	/* Room for the completion? Only the ISR changes the list while we
	 * hold the queue, and it only shortens it. */
	if (so != NULL) {
		mtx_lock(&sc->lock);
		err = sc->qpend[q->qid].n >= ATRIUM_AMD_MAX_PENDING ? EBUSY : 0;
		mtx_unlock(&sc->lock);
		if (err != 0) {
			sx_xunlock(&q->lock);
			return (err);
		}
	}
	/* Stage the frame deadline, program, register, ring: one sequence. */
	if (sc->regs != NULL)
		amd_mmio_write32(sc, regCP_SUBMIT_DEADLINE_NS, deadline_ns);
	err = amd_queue_program(sc, ring_va, engine, vm->vmid, &doorbell_off);
	if (err == 0) {
		q->vmid = vm->vmid;
		q->wptr = n_dwords;
		q->gen++;
		mtx_lock(&sc->lock);
		/* New work, possibly another VM's: a fault reported before
		 * this point belonged to the work before it, unless that work
		 * still owes completions (same VM), which must still fail. */
		if (sc->qpend[q->qid].n == 0)
			sc->qpend[q->qid].faulted = 0;
		if (so != NULL)
			(void)amd_pending_push_locked(sc, q->qid, so,
			    signal_value);
		sc->qpend[q->qid].progress = ticks;
		sc->qpend[q->qid].last_rptr = 0;
		mtx_unlock(&sc->lock);
		bus_write_4(sc->doorbell, doorbell_off, n_dwords);
	}
	sx_xunlock(&q->lock);
	if (err == 0)
		amd_hwq_arm_watchdog(sc);
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
	if (q->owner == NULL &&
	    (err = amd_hwq_wait_idle(sc, q, vm->vmid)) != 0) {
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
			int err = amd_db_grant_create(sc,
			    rman_get_start(sc->doorbell) + q->doorbell_off,
			    nprot, &q->grant_obj);

			if (err != 0) {
				sx_xunlock(&q->lock);
				return (err);
			}
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

static int
amd_iso_sysctl_hang_ms(SYSCTL_HANDLER_ARGS)
{
	struct atrium_amd_softc *sc = arg1;
	int v, err;

	mtx_lock(&sc->lock);
	v = sc->hang_ms;
	mtx_unlock(&sc->lock);
	err = sysctl_handle_int(oidp, &v, 0, req);
	if (err != 0 || req->newptr == NULL)
		return (err);
	if (v < 100 || v > 600000)
		return (EINVAL);
	mtx_lock(&sc->lock);
	sc->hang_ms = v;
	mtx_unlock(&sc->lock);
	return (0);
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
		/* DUPOK: amd_hwq_reset_prepare holds them all, always in index order. */
		sx_init_flags(&q->lock, "atrium_amd_hwq", SX_DUPOK);
		q->qid = i;
		q->engine = (i == ATRIUM_AMD_GFX_QID) ?
		    ATRIUM_GPU_ENGINE_GFX : ATRIUM_GPU_ENGINE_COMPUTE;
		q->doorbell_off = (i == ATRIUM_AMD_GFX_QID) ?
		    ATRIUM_AMD_GFX_DOORBELL : ATRIUM_AMD_COMPUTE_DOORBELL;
	}
	sc->rs_state = amd_rs_dev_init();
	TIMEOUT_TASK_INIT(taskqueue_thread, &sc->defer_task, 0,
	    amd_hwq_reap_task, sc);
	TIMEOUT_TASK_INIT(taskqueue_thread, &sc->watchdog_task, 0,
	    amd_hwq_watchdog_task, sc);
	bzero(sc->qpend, sizeof(sc->qpend));
	sc->watchdog_armed = 0;
	sc->hang_ms = ATRIUM_AMD_HANG_MS;
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
	AMD_ISO_U64("hangs", hangs, "queues the hang watchdog stopped");
	AMD_ISO_U64("vm_faults", vm_faults, "GPU VM fault interrupts");
	AMD_ISO_U64("fences_failed", fences_failed,
	    "owed completions failed with EIO (queue stopped or reset)");
	AMD_ISO_INT("deferred_now", deferred_now, "deferred BO frees pending");
	AMD_ISO_INT("grants_live", grants_live, "doorbell grant objects alive");
#undef AMD_ISO_U64
#undef AMD_ISO_INT
	SYSCTL_ADD_U64(ctx, SYSCTL_CHILDREN(tree), OID_AUTO, "ih_overflows",
	    CTLFLAG_RD, &sc->ih_overflows, 0,
	    "interrupt passes where the device lapped the IH ring");
	SYSCTL_ADD_PROC(ctx, SYSCTL_CHILDREN(tree), OID_AUTO, "hang_ms",
	    CTLTYPE_INT | CTLFLAG_RW | CTLFLAG_MPSAFE, sc, 0,
	    amd_iso_sysctl_hang_ms, "I",
	    "stop a kernel-rung queue after this long without progress");
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
	while (taskqueue_cancel_timeout(taskqueue_thread, &sc->watchdog_task,
	    NULL) != 0)
		taskqueue_drain_timeout(taskqueue_thread, &sc->watchdog_task);
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
	amd_rs_dev_fini(sc->rs_state);
	sc->rs_state = NULL;
	sc->hwq_inited = 0;
}
