/*
 * queue.c — the C half of queue management: register access, the per-queue
 * list of owed completions (shared with the interrupt handler, irq.c), the
 * hang-watchdog and deferred-free timers, and the isolation sysctls.
 *
 * Which client may use which queue, and when, is the Rust front-end's
 * (rs/queue.rs: isolation breaks 1 and 3, the hang check of review §A2.6),
 * as are doorbell grants (rs/doorbell.rs) and deferred BO frees
 * (rs/defer.rs; break 2). The functions here are the small, lock-taking
 * helpers it calls; each takes sc->lock itself where it needs it.
 *
 * Lock order: a queue's lock (rs/queue.rs), then a pager object's lock or the
 * deferred list's lock (rs/defer.rs), then sc->lock. Nothing sleeps holding
 * sc->lock or the deferred list's lock.
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

/* ---------------------------------------------------------------------------
 * Helpers for rs/queue.rs.
 */

uint32_t
amd_rs_q_rptr(struct atrium_amd_softc *sc, uint32_t qid)
{
	return (amd_mmio_read32(sc, ATRIUM_AMD_Q_BASE +
	    qid * ATRIUM_AMD_Q_STRIDE + ATRIUM_AMD_QF_RPTR));
}

void
amd_rs_sc_lock(struct atrium_amd_softc *sc)
{
	mtx_lock(&sc->lock);
}

void
amd_rs_sc_unlock(struct atrium_amd_softc *sc)
{
	mtx_unlock(&sc->lock);
}

void
amd_rs_sc_assert(struct atrium_amd_softc *sc)
{
	mtx_assert(&sc->lock, MA_OWNED);
}

int
amd_rs_ticks(void)
{
	return (ticks);
}

/* hang_ms in ticks, at least one. */
int
amd_rs_hang_ticks_locked(struct atrium_amd_softc *sc)
{
	mtx_assert(&sc->lock, MA_OWNED);
	return (MAX(1, (int)(((int64_t)sc->hang_ms * hz) / 1000)));
}

void
amd_rs_count_locked(struct atrium_amd_softc *sc, int what, uint32_t n)
{
	mtx_assert(&sc->lock, MA_OWNED);
	switch (what) {
	case AMD_RS_CNT_FENCES_FAILED:
		sc->iso.fences_failed += n;
		break;
	case AMD_RS_CNT_DEQUEUES:
		sc->iso.dequeues += n;
		break;
	case AMD_RS_CNT_HANGS:
		sc->iso.hangs += n;
		break;
	}
}

/* The hang check stopped `qid` (called without sc->lock). */
void
amd_rs_q_report_stop(struct atrium_amd_softc *sc, uint32_t qid, uint16_t vmid,
    int what)
{
	if (what == AMD_RS_STOP_FAULT)
		device_printf(sc->dev, "queue %u (VMID %u) faulted: stopped\n",
		    qid, vmid);
	else
		device_printf(sc->dev, "queue %u (VMID %u) made no progress "
		    "for %d ms: stopped\n", qid, vmid, sc->hang_ms);
}

void
amd_rs_q_count(struct atrium_amd_softc *sc, int what)
{
	mtx_lock(&sc->lock);
	switch (what) {
	case AMD_RS_Q_REFUSED:
		sc->iso.queue_refused++;
		break;
	case AMD_RS_Q_WAIT:
		sc->iso.queue_waits++;
		break;
	case AMD_RS_Q_REVOKE:
		sc->iso.revokes++;
		break;
	}
	mtx_unlock(&sc->lock);
}

void
amd_rs_q_deadline(struct atrium_amd_softc *sc, uint32_t ns)
{
	if (sc->regs != NULL)
		amd_mmio_write32(sc, regCP_SUBMIT_DEADLINE_NS, ns);
}

void
amd_rs_q_doorbell(struct atrium_amd_softc *sc, uint32_t off, uint32_t wptr)
{
	bus_write_4(sc->doorbell, off, wptr);
}

/* The doorbell BAR's base, if its pages are registered for grants; else 0. */
uint64_t
amd_rs_doorbell_pa(struct atrium_amd_softc *sc)
{
	if (sc->doorbell == NULL || !sc->db_fict_registered)
		return (0);
	return (rman_get_start(sc->doorbell));
}

/* Point `vmid` at the all-invalid page directory and flush its TLB. */
void
amd_rs_vm_null_pd(struct atrium_amd_softc *sc, uint16_t vmid)
{
	mtx_lock(&sc->lock);
	amd_mmio_write32(sc, regVM_CTX_SELECT, vmid);
	amd_mmio_write32(sc, regVM_CTX_PT_BASE_LO,
	    (uint32_t)(sc->null_pd.gpa & 0xffffffff));
	amd_mmio_write32(sc, regVM_CTX_PT_BASE_HI,
	    (uint32_t)(sc->null_pd.gpa >> 32));
	amd_mmio_write32(sc, regTLB_INVALIDATE, 1);
	mtx_unlock(&sc->lock);
}

/* ---------------------------------------------------------------------------
 * The hang watchdog (review §A2.6).
 *
 * Runs every ~100 ms while any kernel-rung queue is busy; the check itself is
 * amd_rs_watchdog (rs/queue.rs). A queue a client owns is left alone:
 * userspace rings it, so the kernel cannot tell idle from stuck, and the
 * owner can only stall itself.
 */

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
void
amd_rs_watchdog_arm(struct atrium_amd_softc *sc)
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

/* ---------------------------------------------------------------------------
 * Deferred BO frees: the list is rs/defer.rs's; the timer is here.
 */

void
amd_hwq_defer_kick(struct atrium_amd_softc *sc)
{
	taskqueue_enqueue_timeout(taskqueue_thread, &sc->defer_task,
	    AMD_REAP_TICKS);
}

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

static void
amd_hwq_watchdog_task(void *arg, int pending __unused)
{
	struct atrium_amd_softc *sc = arg;
	int active;

	active = amd_rs_watchdog(sc);
	mtx_lock(&sc->lock);
	sc->watchdog_armed = active;
	mtx_unlock(&sc->lock);
	if (active)
		taskqueue_enqueue_timeout(taskqueue_thread, &sc->watchdog_task,
		    AMD_WATCHDOG_TICKS);
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

/* vram_limit_pct / sys_limit_pct: arg2 is the field's offset in the softc. */
static int
amd_iso_sysctl_pct(SYSCTL_HANDLER_ARGS)
{
	struct atrium_amd_softc *sc = arg1;
	int *field = (int *)((char *)sc + arg2);
	int v, err;

	mtx_lock(&sc->lock);
	v = *field;
	mtx_unlock(&sc->lock);
	err = sysctl_handle_int(oidp, &v, 0, req);
	if (err != 0 || req->newptr == NULL)
		return (err);
	if (v < 1 || v > 100)
		return (EINVAL);
	mtx_lock(&sc->lock);
	*field = v;
	mtx_unlock(&sc->lock);
	return (0);
}

int
amd_hwq_init(struct atrium_amd_softc *sc, device_t dev)
{
	struct sysctl_ctx_list *ctx = device_get_sysctl_ctx(dev);
	struct sysctl_oid *tree;
	vm_paddr_t start;
	int err;

	bzero(&sc->iso, sizeof(sc->iso));
	sc->rs_state = amd_rs_dev_init();
	TIMEOUT_TASK_INIT(taskqueue_thread, &sc->defer_task, 0,
	    amd_hwq_reap_task, sc);
	TIMEOUT_TASK_INIT(taskqueue_thread, &sc->watchdog_task, 0,
	    amd_hwq_watchdog_task, sc);
	sc->watchdog_armed = 0;
	sc->hang_ms = ATRIUM_AMD_HANG_MS;
	sc->vram_limit_pct = ATRIUM_AMD_VRAM_LIMIT_PCT;
	sc->sys_limit_pct = ATRIUM_AMD_SYS_LIMIT_PCT;
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
	AMD_ISO_U64("quota_refused", quota_refused,
	    "BO allocations refused: over the owner's per-uid cap");
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
	SYSCTL_ADD_PROC(ctx, SYSCTL_CHILDREN(tree), OID_AUTO, "vram_limit_pct",
	    CTLTYPE_INT | CTLFLAG_RW | CTLFLAG_MPSAFE, sc,
	    offsetof(struct atrium_amd_softc, vram_limit_pct),
	    amd_iso_sysctl_pct, "I",
	    "VRAM one uid's BOs may hold, in % of VRAM (kmod audit G2)");
	SYSCTL_ADD_PROC(ctx, SYSCTL_CHILDREN(tree), OID_AUTO, "sys_limit_pct",
	    CTLTYPE_INT | CTLFLAG_RW | CTLFLAG_MPSAFE, sc,
	    offsetof(struct atrium_amd_softc, sys_limit_pct),
	    amd_iso_sysctl_pct, "I",
	    "System memory one uid's BOs may hold, in % of RAM");
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

	if (!sc->hwq_inited)
		return;
	amd_rs_queues_stop(sc);
	while (taskqueue_cancel_timeout(taskqueue_thread, &sc->watchdog_task,
	    NULL) != 0)
		taskqueue_drain_timeout(taskqueue_thread, &sc->watchdog_task);
	while (taskqueue_cancel_timeout(taskqueue_thread, &sc->defer_task,
	    NULL) != 0)
		taskqueue_drain_timeout(taskqueue_thread, &sc->defer_task);
	(void)amd_rs_reap(sc);
	taskqueue_drain_timeout(taskqueue_thread, &sc->defer_task);
	if (sc->db_fict_registered) {
		start = rman_get_start(sc->doorbell);
		vm_phys_fictitious_unreg_range(start,
		    start + round_page(rman_get_size(sc->doorbell)));
		sc->db_fict_registered = 0;
	}
	amd_dma_page_free(&sc->null_pd);
	amd_rs_dev_fini(sc->rs_state);
	sc->rs_state = NULL;
	sc->hwq_inited = 0;
}
