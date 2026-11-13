/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * si_client.c — the client node on silicon (stage 5c-2a,
 * docs/spec/r9700-bare-metal.md §4.5). The Rust front-end (rs/) is the same
 * one gpusim runs under: VM, BO, queue and syncobj fds, quotas, deferred
 * frees, the hang watchdog. This file is what it reaches on silicon:
 *
 *   VMs     each VM fd gets one of the kernel's VMIDs (1-7; MES owns 8-15)
 *           and its own four-level GPUVM table (gmc12.c, 5b), so a VM's
 *           work can reach only what is bound in it.
 *   BOs     System memory (bus_dma pages, bo.c), or VRAM (5d) from the
 *           base's allocator (vram.c over the range amd_si_vram_init
 *           chose), mapped with VRAM PTEs. Freed VRAM is scrubbed before
 *           reuse by si_vram_clear: CP DMA zero-fills on the kernel queue,
 *           under VMID 0, which reaches VRAM by MC address and which no
 *           client VMID maps. The whole range starts dirty.
 *   Queues  every queue fd is its own MES user queue in its VM (5c-2b,
 *           AMD_UQ_SLOTS of them). Kernel mode: QUEUE_SUBMIT writes an
 *           INDIRECT_BUFFER to the client's blob plus the kernel's own
 *           RELEASE_MEM (EOP interrupt tagged with the queue) on a ring the
 *           client cannot write. User mode (step 3, PRIV_DRIVER only, a
 *           trusted renderer's own path): the client writes the ring and
 *           rings its doorbell itself; QUEUE_SUBMIT is EOPNOTSUPP. A fault
 *           or a hang loses only that queue. Frees wait for MES to preempt
 *           the queues of the VM (SUSPEND + RESUME), never for a fence the
 *           client can write. The v1 ring SUBMIT is retired.
 *   IRQs    the IH ring's consumer (ih7.c): CP EOP IVs retire signalled
 *           submits by fence; GFX UTCL2 fault IVs (client 0xa, src 0) mark
 *           the queue's current work faulted if it runs under the faulting
 *           VMID, and the watchdog then stops it: the stuck waves of that
 *           VMID are killed (SQ_CMD) and the queue fences on (5b), or is
 *           mapped afresh if it does not.
 *
 * The front-end's shared-engine queue model (rs/queue.rs) is gpusim's. Here
 * it only backs the retired 5c-2a kernel-queue path (si_legacy_submit,
 * off): "program" records the ring and VMID, and the queue reports itself
 * fetching until the submit's fence lands.
 *
 * The v1 QUEUE_MAP (doorbell mmap of a shared engine queue) stays gpusim's:
 * EOPNOTSUPP here.
 *
 * GfxOff: GC registers (GFXHUB contexts, fault status, SQ_CMD) read 0 and
 * drop writes while GfxOff is in effect, so every live VM holds GfxOff off
 * (amd_si_gfx_hold), and the queue's setup and teardown hold it too.
 *
 * Locks: the front-end's queue lock (sx) serialises program, kick and stop;
 * sc->lock guards the submit bookkeeping below, which the IV consumer reads
 * from the interrupt thread.
 */
#include "atrium_gpu_amd.h"
#include "atrium_gpu_amd_abi.h"
#include "rs/glue.h"

#define SI_KVMID_FIRST		1u
#define SI_KVMID_LAST		7u
#define SI_STOP_WAIT_US		200000	/* fence after a wave kill (5b: ms) */
#define IH_CLIENT_GFX		0x0au	/* SOC21: GFX (UTCL2 faults, SQ) */
#define IH_CLIENT_GRBM_CP	0x14u
#define IH_SRC_UTCL2_FAULT	0x00u
#define IH_SRC_SQ_INTERRUPT	0xefu	/* 239: SQ interrupt message */
#define IH_SRC_CP_EOP		0xb5u
#define SI_KCQ_DOORBELL		(0x03u << 1)	/* mes12.c KCQ_DOORBELL */
#define SI_UQ_VA		0x200000000ull	/* above the BO window */
#define SI_UQ_VA_STRIDE		0x10000ull
#define SI_SCRUB_TIMEOUT_MS	2000	/* one fill of AMD_KQ_FILL_MAX */
#define SI_UNROUTED_KEEP	4	/* distinct unrouted EOP values kept */

/* One client queue fd's MES user queue (5c-2b). */
struct si_uq_owed {
	struct atrium_amd_syncobj *so;	/* NULL: scrubbed (syncobj closed) */
	uint64_t value;
	uint32_t seq;
};

struct si_uq {
	struct amd_uq *hw;		/* NULL: slot free */
	uint16_t kvmid;			/* the VM's kernel VMID */
	bool	 busy;			/* slot taken (hw may still be NULL) */
	bool	 seen_eop;		/* first routed EOP IV logged */
	bool	 dead;			/* lost (fault, hang): work fails, EIO */
	bool	 evict;			/* lost, still on MES: the task removes it */
	bool	 user;			/* user mode: the client's ring (step 3) */
	uint32_t prog_rptr;		/* user mode's hang clock: rptr at prog_tick */
	uint32_t prog_fence;		/* hang clock: fence at prog_tick */
	int	 prog_tick;
	struct si_uq_owed owed[ATRIUM_AMD_MAX_PENDING];
	u_int	 head, n;
	uint64_t submits;
};

/* The 5c-2a path (client SUBMIT on the kernel queue): off from 5c-2b. */
static bool si_legacy_submit = false;

struct amd_si_client {
	struct atrium_amd_softc *sc;
	struct amd_gart *gt;
	struct amd_ih7 *ih;
	struct amd_kq *kq;
	bool	 kq_held;		/* GfxOff hold for the queue's setup */

	/* sc->lock */
	uint32_t vmids;			/* kernel VMIDs in use, bit n = VMID n */
	uint64_t ring_va;		/* programmed: the next submit's ring */
	uint16_t vmid;			/* ... and its VMID; 0 = none */
	uint16_t last_vmid;		/* VMID of the last submit */
	uint32_t last_seq;		/* its fence value */
	uint32_t last_ndw;		/* its length (the front-end's wptr) */
	uint32_t sig[ATRIUM_AMD_MAX_PENDING]; /* fences of signalled submits */
	u_int	 sig_head, sig_n;
	bool	 broken;		/* the queue could not be recovered */

	/* 5c-2b: client queue fds on MES user queues (uq_sx serialises
	 * create and destroy; the slots' bookkeeping is under sc->lock). */
	struct sx uq_sx;
	struct si_uq uq[AMD_UQ_SLOTS];
	u_int	 uq_live;
	uint32_t uq_int_saved[4];	/* MEC1 pipes' INT_CNTL before */
	struct callout uq_poll;
	struct task uq_task;		/* evicts lost queues (taskqueue_thread) */
	uint64_t uq_hangs, uq_faults;
	uint64_t uq_preempts, uq_preempt_fails;	/* the free rule */
	uint64_t uq_eop_preempt;	/* MES's suspend/resume EOPs */
	uint64_t uq_eop_routed, uq_eop_unrouted, uq_eop_mes, uq_retired_iv,
		 uq_retired_poll;

	/* The IV consumer's batch (interrupt thread only). */
	u_int	 b_eop, b_fault, b_sq, b_other;
	uint32_t b_uq;			/* user queues named by an EOP */
	uint32_t b_fault_vmids;		/* every VMID that faulted */
	bool	 b_uq_scan;		/* an EOP named no queue: check all */
	uint32_t b_unrouted_db;		/* ... and the doorbell it gave */
	/* The first distinct unrouted src_data[0] values and their counts
	 * (the IV thread writes, the close log reads; diagnostic). */
	uint32_t unr_val[SI_UNROUTED_KEEP];
	uint64_t unr_cnt[SI_UNROUTED_KEEP];
	uint64_t scrub_fills, scrub_busy_waits;
	uint16_t b_fault_vmid;
	uint64_t b_fault_addr;
	struct timeval log_last;
	int	 log_pps;
	struct timeval flog_last;	/* VM fault lines: their own budget */
	int	 flog_pps;
};

static bool
fence_passed(uint32_t fence, uint32_t seq)
{
	return ((int32_t)(fence - seq) >= 0);
}

/* ---- the IV consumer (ih7.c), interrupt thread ---- */

/*
 * Retire what user queue `slot` owes up to its fence (sc->lock held).
 * The fence is in the client's VM: a client that writes it signals its
 * own syncobjs early, and nobody else's.
 */
static u_int
si_uq_retire(struct amd_si_client *c, u_int slot)
{
	struct si_uq *u = &c->uq[slot];
	struct si_uq_owed *o;
	uint32_t fence;
	u_int n = 0;

	mtx_assert(&c->sc->lock, MA_OWNED);
	if (u->hw == NULL)
		return (0);
	fence = amd_mes12_uq_fence(u->hw);
	while (u->n > 0 && fence_passed(fence, u->owed[u->head].seq)) {
		o = &u->owed[u->head];
		if (o->so != NULL)
			amd_syncobj_signal(o->so, o->value);
		o->so = NULL;
		u->head = (u->head + 1) % ATRIUM_AMD_MAX_PENDING;
		u->n--;
		n++;
	}
	return (n);
}

static bool
si_uq_owes(struct amd_si_client *c)
{
	u_int i;

	for (i = 0; i < AMD_UQ_SLOTS; i++)
		if (c->uq[i].hw != NULL && (c->uq[i].n > 0 ||
		    (c->uq[i].user && !c->uq[i].dead)))
			return (true);
	return (false);
}

/*
 * A UTCL2 fault on a MES VMID (8-15): the user queue MES runs under it now
 * is lost. What it owes fails (EIO) and its later submits are refused; its
 * close takes it off the GPU (resetting the HQD if it is wedged). Its
 * client's other queues and everyone else's run on. sc->lock held.
 */
static void
si_uq_faulted(struct amd_si_client *c, uint32_t vmids, bool log)
{
	struct si_uq *u;
	u_int i, vmid;

	mtx_assert(&c->sc->lock, MA_OWNED);
	for (i = 0; i < AMD_UQ_SLOTS; i++) {
		u = &c->uq[i];
		if (u->hw == NULL || u->dead)
			continue;
		vmid = amd_mes12_uq_hqd_vmid(u->hw);
		if (vmid < 8 || (vmids & (1u << vmid)) == 0)
			continue;
		(void)si_uq_retire(c, i);	/* what fenced before it */
		while (u->n > 0) {
			if (u->owed[u->head].so != NULL)
				amd_syncobj_fail(u->owed[u->head].so, EIO);
			u->owed[u->head].so = NULL;
			u->head = (u->head + 1) % ATRIUM_AMD_MAX_PENDING;
			u->n--;
		}
		u->dead = true;
		u->evict = true;
		c->uq_faults++;
		taskqueue_enqueue(taskqueue_thread, &c->uq_task);
		if (log)
			device_printf(c->sc->dev, "si: uq%u faulted (VMID %u, "
			    "%#jx): its work fails, the queue is lost\n", i,
			    vmid, (uintmax_t)c->b_fault_addr);
	}
}

/* The card was reset: every user queue is lost (EIO). sc->lock held. */
static void
si_uq_lost_all(struct amd_si_client *c)
{
	struct si_uq *u;
	u_int i;

	mtx_assert(&c->sc->lock, MA_OWNED);
	for (i = 0; i < AMD_UQ_SLOTS; i++) {
		u = &c->uq[i];
		while (u->n > 0) {
			if (u->owed[u->head].so != NULL)
				amd_syncobj_fail(u->owed[u->head].so, EIO);
			u->owed[u->head].so = NULL;
			u->head = (u->head + 1) % ATRIUM_AMD_MAX_PENDING;
			u->n--;
		}
		u->dead = true;
	}
}

/*
 * The hang clock (sc->lock held): a queue that owes completions and whose
 * fence has not moved for hang_ms is lost. Its work fails (EIO) and the
 * task takes it off MES (waves killed, HQD reset if need be).
 */
static void
si_uq_hang_check(struct amd_si_client *c, u_int slot)
{
	struct atrium_amd_softc *sc = c->sc;
	struct si_uq *u = &c->uq[slot];
	uint32_t fence, rptr;
	int limit;

	if (u->hw == NULL || u->dead)
		return;
	if (u->user) {
		/* Behind (wptr not fetched, fence short of what the client
		 * emitted), and neither rptr nor fence moving. */
		if (!amd_mes12_uq_user_busy(u->hw, &rptr, &fence) ||
		    rptr != u->prog_rptr || fence != u->prog_fence) {
			u->prog_rptr = rptr;
			u->prog_fence = fence;
			u->prog_tick = ticks;
			return;
		}
	} else if (u->n == 0)
		return;
	else if ((fence = amd_mes12_uq_fence(u->hw)) != u->prog_fence) {
		u->prog_fence = fence;
		u->prog_tick = ticks;
		return;
	}
	limit = MAX(1, (int)(((int64_t)sc->hang_ms * hz) / 1000));
	if (ticks - u->prog_tick < limit)
		return;
	device_printf(sc->dev, "si: uq%u hung%s: fence %u, no progress for "
	    "%d ms; its work fails, the queue is lost\n", slot,
	    u->user ? " (user mode)" : "", fence, sc->hang_ms);
	while (u->n > 0) {
		if (u->owed[u->head].so != NULL)
			amd_syncobj_fail(u->owed[u->head].so, EIO);
		u->owed[u->head].so = NULL;
		u->head = (u->head + 1) % ATRIUM_AMD_MAX_PENDING;
		u->n--;
	}
	u->dead = true;
	u->evict = true;
	c->uq_hangs++;
	sc->iso.hangs++;
	taskqueue_enqueue(taskqueue_thread, &c->uq_task);
}

/*
 * Take lost queues off MES now, not at their close: a faulted wave or a
 * hung CP keeps the engine busy (and the card's power up) for as long as
 * the queue stays mapped. The fd stays open and lost; its close frees.
 */
static void
si_uq_evict_task(void *arg, int pending __unused)
{
	struct amd_si_client *c = arg;
	struct atrium_amd_softc *sc = c->sc;
	struct amd_uq *hw;
	u_int i;
	int err;

	sx_xlock(&c->uq_sx);
	for (i = 0; i < AMD_UQ_SLOTS; i++) {
		mtx_lock(&sc->lock);
		hw = c->uq[i].evict ? c->uq[i].hw : NULL;
		c->uq[i].evict = false;
		mtx_unlock(&sc->lock);
		if (hw == NULL || !amd_si_usable(sc))
			continue;
		if ((err = amd_mes12_uq_evict(hw)) != 0) {
			amd_si_lose(sc, "a lost user queue MES will not remove");
			mtx_lock(&sc->lock);
			si_uq_lost_all(c);
			mtx_unlock(&sc->lock);
			break;
		}
		device_printf(sc->dev, "si: uq%u evicted (lost)\n", i);
	}
	sx_xunlock(&c->uq_sx);
}

/*
 * The fallback: while a user queue owes completions, look at the fences
 * every 50 ms, so a missing EOP interrupt costs latency, not a hang. The
 * counts tell the box log which path retired them.
 */
static void
si_uq_poll(void *arg)
{
	struct amd_si_client *c = arg;
	u_int i;

	mtx_assert(&c->sc->lock, MA_OWNED);
	for (i = 0; i < AMD_UQ_SLOTS; i++) {
		c->uq_retired_poll += si_uq_retire(c, i);
		si_uq_hang_check(c, i);
	}
	if (si_uq_owes(c))
		callout_reset(&c->uq_poll, hz / 20, si_uq_poll, c);
}


static void
si_iv_batch_end(struct amd_si_client *c)
{
	struct atrium_amd_softc *sc = c->sc;
	const u_int q = ATRIUM_AMD_COMPUTE_QID;
	uint32_t fence, eop = 0, status;
	uint64_t addr;
	bool fault = false, log, flog = false;
	int i;

	log = ppsratecheck(&c->log_last, &c->log_pps, 5);
	/* A fault always gets its line (address, VMID, decoded status) unless
	 * faults themselves arrive faster than 10 a second: box runs
	 * 05fdaebf and 955528b9 lost queues to faults with nothing logged. */
	if (c->b_fault != 0)
		flog = ppsratecheck(&c->flog_last, &c->flog_pps, 10);
	mtx_lock(&sc->lock);
	for (i = 0; i < ATRIUM_AMD_NHWQ; i++) {
		sc->ih_eop_ring[i] = 0;
		sc->ih_fault_ring[i] = 0;
	}
	if (c->b_fault != 0) {
		/* iso.vm_faults: counted once, by amd_fault_handler when the
		 * fault goes to it, else below. */
		/* Re-arm the latch (GC registers: valid while VMs hold
		 * GfxOff off, and only a VM's work can fault here). */
		if (flog)
			device_printf(sc->dev, "si: VM fault: %u IV(s), VMIDs "
			    "%#x, first VMID %u at %#jx\n", c->b_fault,
			    c->b_fault_vmids, c->b_fault_vmid,
			    (uintmax_t)c->b_fault_addr);
		if (c->vmids != 0 && amd_gmc12_fault_take(c->gt, &status,
		    &addr) && flog)
			amd_gmc12_fault_decode(c->gt, status, "client fault");
		if (c->b_fault_vmid != 0 && c->b_fault_vmid == c->last_vmid &&
		    c->kq != NULL && !fence_passed(amd_mes12_kq_fence(c->kq),
		    c->last_seq)) {
			sc->ih_fault_ring[q] = c->b_fault;
			sc->ih_fault_vmid[q] = c->b_fault_vmid;
			sc->ih_fault_page[q] = (uint32_t)(c->b_fault_addr >> 12);
			fault = true;
		} else if ((c->b_fault_vmids & 0xff00u) != 0) {
			si_uq_faulted(c, c->b_fault_vmids & 0xff00u, flog);
		} else if (flog)
			device_printf(sc->dev, "si: VM fault on VMID %u at "
			    "%#jx, not the running submit's VMID %u\n",
			    c->b_fault_vmid, (uintmax_t)c->b_fault_addr,
			    c->last_vmid);
	}
	if (c->kq != NULL && (c->b_eop != 0 || fault)) {
		fence = amd_mes12_kq_fence(c->kq);
		while (c->sig_n > 0 && fence_passed(fence, c->sig[c->sig_head])) {
			c->sig_head = (c->sig_head + 1) % ATRIUM_AMD_MAX_PENDING;
			c->sig_n--;
			eop++;
		}
	}
	/* User queues: an EOP names its queue by doorbell (5c-2b). */
	if (c->b_uq != 0 || c->b_uq_scan) {
		for (i = 0; i < AMD_UQ_SLOTS; i++)
			if (c->b_uq_scan || (c->b_uq & (1u << i)) != 0)
				c->uq_retired_iv += si_uq_retire(c, i);
	}
	sc->ih_eop_ring[q] = eop;
	/* The fault first, so the completions it covers fail (EIO). */
	if (fault && sc->ih_handler[ATRIUM_AMD_IH_CAUSE_VM_FAULT] != NULL)
		sc->ih_handler[ATRIUM_AMD_IH_CAUSE_VM_FAULT](sc, c->b_fault);
	else
		sc->iso.vm_faults += c->b_fault;
	if (eop != 0 && sc->ih_handler[ATRIUM_AMD_IH_CAUSE_EOP] != NULL)
		sc->ih_handler[ATRIUM_AMD_IH_CAUSE_EOP](sc, eop);
	mtx_unlock(&sc->lock);
	if (c->b_sq != 0 && log)
		device_printf(sc->dev, "si: %u SQ interrupt message(s)\n",
		    c->b_sq);
	if (c->b_other != 0 && log)
		device_printf(sc->dev, "si: %u other IV(s)\n", c->b_other);
	if (c->b_eop + c->b_fault + c->b_sq + c->b_other != 0)
		atomic_add_int(&sc->irq_count, 1);
	if (c->b_uq_scan && log)
		device_printf(sc->dev, "si: EOP IV with src_data[0] %#x: no "
		    "queue of ours; checked every user queue\n",
		    c->b_unrouted_db);
	c->b_eop = c->b_fault = c->b_sq = c->b_other = 0;
	c->b_fault_vmid = 0;
	c->b_uq = 0;
	c->b_uq_scan = false;
	c->b_fault_vmids = 0;
}

/*
 * An EOP IV's src_data[0] is the RELEASE_MEM's INT_CTXID, which our
 * submits set: AMD_UQ_CTXID(doorbell) for a user queue (first box run:
 * the raw value is not the doorbell unless the packet puts it there),
 * AMD_KQ_CTXID for the kernel queue. Anything else checks every queue.
 */
static void
si_iv_eop_route(struct amd_si_client *c, uint32_t ctx)
{
	uint32_t db, slot;

	if ((ctx & 0xff000000u) == AMD_UQ_CTXID_FLAG) {
		db = ctx & 0x00ffffffu;
		if ((db & 0x3ffu) == 0 && db >= AMD_UQ_DOORBELL_USER(0))
			slot = (db >> 10) - 2u;		/* user mode */
		else if (db >= AMD_UQ_DOORBELL(0) && (db & 1) == 0)
			slot = (db >> 1) - 0x10u;
		else
			slot = AMD_UQ_SLOTS;
		if (slot < AMD_UQ_SLOTS) {
			c->b_uq |= 1u << slot;
			c->uq_eop_routed++;
			if (!c->uq[slot].seen_eop) {
				c->uq[slot].seen_eop = true;
				device_printf(c->sc->dev, "si: uq%u: first EOP "
				    "IV, src_data[0] %#x\n", slot, ctx);
			}
			return;
		}
	}
	if (ctx == AMD_KQ_CTXID)
		return;
	/* A bare doorbell (no AMD_UQ_CTXID_FLAG): MES's EOP for a SUSPEND or
	 * RESUME of that queue (5d run: they track the free-rule preemptions
	 * one for one). Nothing of the queue's own work to retire. */
	if ((ctx & 0xff000000u) == 0 && ((ctx >= AMD_UQ_DOORBELL(0) &&
	    ctx < AMD_UQ_DOORBELL(AMD_UQ_SLOTS) && (ctx & 1) == 0) ||
	    (ctx >= AMD_UQ_DOORBELL_USER(0) &&
	    ctx < AMD_UQ_DOORBELL_USER(AMD_UQ_SLOTS) && (ctx & 0x3ffu) == 0))) {
		c->uq_eop_preempt++;
		return;
	}
	/* MES's own scheduler EOPs (seen on silicon: 0x2000000b,
	 * 0x20000002, 0x20000003): not a queue of ours, nothing to scan. */
	if ((ctx & 0xff000000u) == AMD_MES_CTXID_FLAG) {
		c->uq_eop_mes++;
		return;
	}
	c->uq_eop_unrouted++;
	c->b_uq_scan = true;
	c->b_unrouted_db = ctx;
	for (slot = 0; slot < SI_UNROUTED_KEEP; slot++) {
		if (c->unr_cnt[slot] != 0 && c->unr_val[slot] != ctx)
			continue;
		c->unr_val[slot] = ctx;
		c->unr_cnt[slot]++;
		break;
	}
}

static void
si_iv(void *arg, const uint32_t *iv)
{
	struct amd_si_client *c = arg;
	uint32_t client, src;

	if (iv == NULL) {
		si_iv_batch_end(c);
		return;
	}
	client = iv[0] & 0xff;
	src = (iv[0] >> 8) & 0xff;
	if (client == IH_CLIENT_GRBM_CP && src == IH_SRC_CP_EOP) {
		c->b_eop++;
		si_iv_eop_route(c, iv[4]);
	}
	else if (client == IH_CLIENT_GFX && src == IH_SRC_UTCL2_FAULT) {
		/* gmc_v12_0_process_interrupt: VMID in dword 0, the page in
		 * data 0 and bits 47:44 in data 1. */
		c->b_fault_vmids |= 1u << ((iv[0] >> 24) & 0xf);
		if (c->b_fault++ == 0) {
			c->b_fault_vmid = (iv[0] >> 24) & 0xf;
			c->b_fault_addr = (uint64_t)iv[4] << 12 |
			    (uint64_t)(iv[5] & 0xf) << 44;
		}
	} else if (client == IH_CLIENT_GFX && src == IH_SRC_SQ_INTERRUPT)
		c->b_sq++;
	else
		c->b_other++;
}

/* ---- queue hardware for rs/queue.rs (via cp.c and queue.c) ---- */

int
amd_si_q_program(struct atrium_amd_softc *sc, uint64_t ring_va,
    uint32_t engine, uint16_t vmid, uint32_t *doorbell_off)
{
	struct amd_si_client *c = sc->si_client;

	/*
	 * 5c-2b: client work runs on its own MES user queue (QUEUE_CREATE,
	 * QUEUE_SUBMIT on the queue fd). The kernel queue is the kernel's.
	 */
	if (!si_legacy_submit)
		return (EOPNOTSUPP);
	if (engine != ATRIUM_GPU_ENGINE_COMPUTE)
		return (EOPNOTSUPP);
	if (c == NULL || c->kq == NULL || !amd_si_usable(sc))
		return (ENXIO);
	if (vmid < SI_KVMID_FIRST || vmid > SI_KVMID_LAST || (ring_va & 3))
		return (EINVAL);
	mtx_lock(&sc->lock);
	if (c->broken) {
		mtx_unlock(&sc->lock);
		return (EIO);
	}
	c->ring_va = ring_va;
	c->vmid = vmid;
	mtx_unlock(&sc->lock);
	*doorbell_off = 0;
	return (0);
}

void
amd_si_q_kick(struct atrium_amd_softc *sc, uint32_t n_dwords, bool signalled)
{
	struct amd_si_client *c = sc->si_client;
	uint32_t seq;
	int err;

	if (c == NULL || c->kq == NULL)
		return;
	mtx_lock(&sc->lock);
	if (c->vmid == 0 || c->broken) {
		err = ENXIO;
	} else if (signalled && c->sig_n >= ATRIUM_AMD_MAX_PENDING) {
		err = EBUSY;
	} else if ((err = amd_mes12_kq_submit(c->kq, c->ring_va, n_dwords,
	    c->vmid, &seq)) == 0) {
		if (signalled) {
			c->sig[(c->sig_head + c->sig_n) %
			    ATRIUM_AMD_MAX_PENDING] = seq;
			c->sig_n++;
		}
		c->last_vmid = c->vmid;
		c->last_seq = seq;
		c->last_ndw = n_dwords;
	}
	mtx_unlock(&sc->lock);
	/* Not run: what it owes never retires, and the watchdog stops the
	 * queue after hang_ms (the submitter sees EIO). */
	if (err != 0)
		device_printf(sc->dev, "si: submit not run (%d)\n", err);
}

/*
 * The front-end's read pointer: the last submit's length once its fence
 * landed, 0 before. So the queue is "fetching" until the work is done.
 */
uint32_t
amd_si_q_rptr(struct atrium_amd_softc *sc, uint32_t qid)
{
	struct amd_si_client *c = sc->si_client;
	uint32_t r = 0;

	if (c == NULL || c->kq == NULL || qid != ATRIUM_AMD_COMPUTE_QID)
		return (0);
	mtx_lock(&sc->lock);
	if (c->last_seq == 0 || fence_passed(amd_mes12_kq_fence(c->kq),
	    c->last_seq))
		r = c->last_ndw;
	mtx_unlock(&sc->lock);
	return (r);
}

/*
 * Stop the queue (a fault, a hang, the VM going away, detach): drop what
 * it owed (the front-end fails those completions) and make sure nothing of
 * the last submit still runs. If its fence has not landed, kill the waves
 * of its VMID and wait for the fence; if it still does not come, map the
 * queue afresh.
 */
void
amd_si_q_stop(struct atrium_amd_softc *sc, uint32_t qid)
{
	struct amd_si_client *c = sc->si_client;
	uint32_t seq;
	uint16_t vmid;
	int us, err;

	if (c == NULL || c->kq == NULL || qid != ATRIUM_AMD_COMPUTE_QID)
		return;
	mtx_lock(&sc->lock);
	c->vmid = 0;
	c->sig_n = 0;
	vmid = c->last_vmid;
	seq = c->last_seq;
	mtx_unlock(&sc->lock);
	if (seq == 0 || fence_passed(amd_mes12_kq_fence(c->kq), seq))
		return;
	if (!amd_si_usable(sc))
		return;
	if (amd_si_gfx_hold(sc) != 0)
		return;
	err = amd_gfx12_sq_kill_vmid(sc, vmid);
	for (us = 0; us < SI_STOP_WAIT_US &&
	    !fence_passed(amd_mes12_kq_fence(c->kq), seq); us += 100)
		DELAY(100);
	if (fence_passed(amd_mes12_kq_fence(c->kq), seq)) {
		device_printf(sc->dev, "si: queue stopped: VMID %u's waves "
		    "killed (%d), fence %u after %d us\n", vmid, err, seq, us);
	} else {
		amd_mes12_kq_report(c->kq, "stop: no fence after wave kill");
		err = amd_mes12_kq_reset(c->kq);
		mtx_lock(&sc->lock);
		c->last_seq = 0;
		c->last_ndw = 0;
		c->broken = err != 0;
		mtx_unlock(&sc->lock);
		device_printf(sc->dev, "si: queue %s\n", err == 0 ?
		    "mapped afresh" : "could not be recovered; submits fail "
		    "until detach");
	}
	amd_si_gfx_release(sc);
}

/* The VM's VMID points at nothing: every access by it faults. */
void
amd_si_vm_unbind(struct atrium_amd_softc *sc, uint16_t vmid)
{
	struct amd_si_client *c = sc->si_client;
	int err;

	if (c == NULL || vmid < SI_KVMID_FIRST || vmid > SI_KVMID_LAST ||
	    !amd_si_usable(sc))
		return;
	if ((err = amd_gmc12_vm_bind(c->gt, vmid, NULL)) != 0)
		device_printf(sc->dev, "si: VMID %u unbind failed (%d)\n",
		    vmid, err);
}

/* ---- client queue fds on MES user queues (5c-2b; rs/uqueue.rs) ---- */

/* Kernel VMIDs whose VM has a live (or leaked) user queue. */
static uint32_t
si_uq_vmids_locked(struct amd_si_client *c)
{
	uint32_t mask = 0;
	u_int i;

	for (i = 0; i < AMD_UQ_SLOTS; i++)
		if (c->uq[i].busy && c->uq[i].kvmid != 0)
			mask |= 1u << c->uq[i].kvmid;
	return (mask);
}

uint32_t
amd_si_uq_vmids(struct atrium_amd_softc *sc)
{
	struct amd_si_client *c = sc->si_client;
	uint32_t mask;

	if (!amd_is_si(sc) || c == NULL)
		return (0);
	mtx_lock(&sc->lock);
	mask = si_uq_vmids_locked(c);
	mtx_unlock(&sc->lock);
	return (mask);
}

/*
 * A PTE of the VM changed: flush its kernel VMID, and, while one of its
 * queues may run under a MES VMID (8-15), those too (MES does not tell us
 * which; the flush is per VMID, so flush them all).
 */
static int
si_vm_flush(struct amd_si_client *c, uint16_t kvmid)
{
	uint32_t mask;
	u_int vmid;
	int err;

	err = amd_gmc12_vm_flush(c->gt, kvmid);
	mtx_lock(&c->sc->lock);
	mask = si_uq_vmids_locked(c);
	mtx_unlock(&c->sc->lock);
	if ((mask & (1u << kvmid)) != 0)
		for (vmid = 8; vmid < 16; vmid++)
			if (amd_gmc12_vm_flush(c->gt, vmid) != 0)
				err = ETIMEDOUT;
	return (err);
}

/*
 * `upg`/`upa`: a user-mode queue on the caller's pages (rs/uqueue.rs maps
 * them, and the doorbell page, into the client); NULL for kernel mode.
 */
int
amd_si_uq_create(struct atrium_amd_softc *sc, struct atrium_amd_vm *vm,
    uint32_t prio, uint8_t *upg, uint64_t upa, int *slotp)
{
	struct amd_si_client *c = sc->si_client;
	struct amd_uq *hw;
	struct si_uq *u;
	u_int slot;
	int err;

	if (!amd_is_si(sc))
		return (EOPNOTSUPP);
	if (c == NULL || !amd_si_usable(sc))
		return (ENXIO);
	if (vm->hw12 == NULL || vm->vmid < SI_KVMID_FIRST ||
	    vm->vmid > SI_KVMID_LAST)
		return (EINVAL);
	sx_xlock(&c->uq_sx);
	mtx_lock(&sc->lock);
	for (slot = 0; slot < AMD_UQ_SLOTS; slot++)
		if (!c->uq[slot].busy)
			break;
	if (slot == AMD_UQ_SLOTS) {
		mtx_unlock(&sc->lock);
		sx_xunlock(&c->uq_sx);
		return (ENOSPC);
	}
	u = &c->uq[slot];
	memset(u, 0, sizeof(*u));
	u->busy = true;
	u->kvmid = vm->vmid;
	mtx_unlock(&sc->lock);

	if (c->uq_live == 0)
		amd_mes12_uq_ints(amd_si_mes(sc), true, c->uq_int_saved);
	/* MES priority: NORMAL (1) for every client in 5c-2b. */
	err = amd_mes12_uq_create(amd_si_mes(sc), vm->hw12, vm->vmid, slot,
	    SI_UQ_VA + slot * SI_UQ_VA_STRIDE, prio + 1, upg, (vm_paddr_t)upa,
	    &hw);
	mtx_lock(&sc->lock);
	if (err != 0) {
		u->busy = false;
		u->kvmid = 0;
	} else {
		u->hw = hw;
		u->user = upg != NULL;
		u->prog_tick = ticks;
		c->uq_live++;
		/* User mode: the hang clock runs for as long as it lives. */
		if (u->user && !callout_pending(&c->uq_poll))
			callout_reset(&c->uq_poll, hz / 20, si_uq_poll, c);
	}
	mtx_unlock(&sc->lock);
	if (err != 0 && c->uq_live == 0)
		amd_mes12_uq_ints(amd_si_mes(sc), false, c->uq_int_saved);
	sx_xunlock(&c->uq_sx);
	if (err == 0)
		*slotp = (int)slot;
	return (err);
}

/*
 * The queue fd's last close. What has fenced is signalled, the rest is
 * cancelled (ECANCELED), then the queue comes off MES. EBUSY: MES kept it;
 * its memory stays allocated and the slot taken, and the caller keeps the
 * VM (its page tables) alive for good.
 */
int
amd_si_uq_destroy(struct atrium_amd_softc *sc, int slot)
{
	struct amd_si_client *c = sc->si_client;
	struct si_uq *u;
	struct amd_uq *hw;
	uint64_t submits;
	uint16_t kvmid;
	u_int done;
	int err;

	if (c == NULL || slot < 0 || slot >= AMD_UQ_SLOTS)
		return (EINVAL);
	u = &c->uq[slot];
	sx_xlock(&c->uq_sx);
	mtx_lock(&sc->lock);
	done = si_uq_retire(c, slot);
	c->uq_retired_iv += done;
	while (u->n > 0) {
		if (u->owed[u->head].so != NULL)
			amd_syncobj_fail(u->owed[u->head].so, ECANCELED);
		u->owed[u->head].so = NULL;
		u->head = (u->head + 1) % ATRIUM_AMD_MAX_PENDING;
		u->n--;
	}
	hw = u->hw;
	u->hw = NULL;
	kvmid = u->kvmid;
	submits = u->submits;
	mtx_unlock(&sc->lock);
	if (hw == NULL) {
		sx_xunlock(&c->uq_sx);
		return (EINVAL);
	}
	if (!amd_si_usable(sc)) {
		/* Lost or failing: make sure it is reset with DMA off. */
		amd_si_lose(sc, "card not usable at a queue's close");
		amd_mes12_uq_free_lost(hw);
		err = 0;
	} else if ((err = amd_mes12_uq_destroy(hw, kvmid)) != 0) {
		/*
		 * MES kept the queue even after an HQD reset: the last rung.
		 * Reset the card, fail what every queue owes, and free.
		 */
		amd_si_lose(sc, "a user queue MES will not remove");
		mtx_lock(&sc->lock);
		si_uq_lost_all(c);
		mtx_unlock(&sc->lock);
		amd_mes12_uq_free_lost(hw);
		err = 0;
	}
	device_printf(sc->dev, "si: uq%d closed: %ju submits; EOP IVs "
	    "routed %ju, unrouted %ju, MES own %ju, MES preemption %ju; "
	    "retired by IV %ju, by poll %ju; queues lost to faults %ju, "
	    "hangs %ju; preempted for frees %ju (failed %ju)\n",
	    slot, (uintmax_t)submits, (uintmax_t)c->uq_eop_routed,
	    (uintmax_t)c->uq_eop_unrouted, (uintmax_t)c->uq_eop_mes,
	    (uintmax_t)c->uq_eop_preempt,
	    (uintmax_t)c->uq_retired_iv,
	    (uintmax_t)c->uq_retired_poll, (uintmax_t)c->uq_faults,
	    (uintmax_t)c->uq_hangs, (uintmax_t)c->uq_preempts,
	    (uintmax_t)c->uq_preempt_fails);
	if (c->unr_cnt[0] != 0)
		device_printf(sc->dev, "si: unrouted EOP src_data[0]: %#x x%ju, "
		    "%#x x%ju, %#x x%ju, %#x x%ju\n",
		    c->unr_val[0], (uintmax_t)c->unr_cnt[0],
		    c->unr_val[1], (uintmax_t)c->unr_cnt[1],
		    c->unr_val[2], (uintmax_t)c->unr_cnt[2],
		    c->unr_val[3], (uintmax_t)c->unr_cnt[3]);
	if (err == 0) {
		mtx_lock(&sc->lock);
		u->busy = false;
		u->kvmid = 0;
		c->uq_live--;
		mtx_unlock(&sc->lock);
		if (c->uq_live == 0 && amd_si_usable(sc))
			amd_mes12_uq_ints(amd_si_mes(sc), false,
			    c->uq_int_saved);
	}
	sx_xunlock(&c->uq_sx);
	return (err);
}

int
amd_si_uq_submit(struct atrium_amd_softc *sc, int slot, uint64_t ib,
    uint32_t ib_dw, struct atrium_amd_syncobj *so, uint64_t value,
    uint64_t *seqno)
{
	struct amd_si_client *c = sc->si_client;
	struct si_uq *u;
	uint32_t seq;
	int err;

	if (c == NULL || slot < 0 || slot >= AMD_UQ_SLOTS)
		return (EINVAL);
	u = &c->uq[slot];
	mtx_lock(&sc->lock);
	if (u->hw == NULL)
		err = ENXIO;
	else if (u->user)
		err = EOPNOTSUPP;		/* the client rings it itself */
	else if (u->dead || !amd_si_usable(sc))
		err = EIO;
	else if (so != NULL && u->n >= ATRIUM_AMD_MAX_PENDING &&
	    si_uq_retire(c, slot) == 0)
		err = EBUSY;
	else if ((err = amd_mes12_uq_submit(u->hw, ib, ib_dw, &seq)) == 0) {
		u->submits++;
		*seqno = u->submits;
		if (so != NULL) {
			if (u->n == 0) {
				u->prog_fence = amd_mes12_uq_fence(u->hw);
				u->prog_tick = ticks;
			}
			u->owed[(u->head + u->n) % ATRIUM_AMD_MAX_PENDING] =
			    (struct si_uq_owed){ so, value, seq };
			u->n++;
			if (!callout_pending(&c->uq_poll))
				callout_reset(&c->uq_poll, hz / 20,
				    si_uq_poll, c);
		}
	}
	mtx_unlock(&sc->lock);
	return (err);
}

/*
 * The free rule (spec §4.5.1 step 4): a BO of the VMs in `mask` (kernel
 * VMIDs) lost its PTEs, flushed on every MES VMID. Preempt each of their
 * user queues once (MES SUSPEND to a kernel fence, then RESUME), so what
 * they had in flight is done with the old translations. Returns the VMIDs
 * not proven that way (a queue MES would not suspend, a lost queue, a
 * leaked one): the BO waits for their queues to go (defer.rs).
 */
uint32_t
amd_si_uq_preempt(struct atrium_amd_softc *sc, uint32_t mask)
{
	struct amd_si_client *c = sc->si_client;
	struct amd_uq *hw;
	uint32_t left = 0, bit;
	u_int i;
	bool busy, dead;
	int err;

	if (!amd_is_si(sc) || c == NULL)
		return (mask);
	sx_xlock(&c->uq_sx);
	for (i = 0; i < AMD_UQ_SLOTS; i++) {
		mtx_lock(&sc->lock);
		bit = c->uq[i].kvmid < 32 ? 1u << c->uq[i].kvmid : 0;
		busy = c->uq[i].busy && (mask & bit) != 0;
		hw = c->uq[i].hw;
		dead = c->uq[i].dead;
		mtx_unlock(&sc->lock);
		if (!busy)
			continue;
		if (hw == NULL || dead || !amd_si_usable(sc)) {
			left |= bit;
			continue;
		}
		err = amd_mes12_uq_preempt(hw);
		mtx_lock(&sc->lock);
		if (err != 0) {
			left |= bit;
			c->uq_preempt_fails++;
		} else
			c->uq_preempts++;
		mtx_unlock(&sc->lock);
		if (err != 0)
			device_printf(sc->dev, "si: uq%u could not be preempted "
			    "(%d); the BO waits for its queues to close\n", i,
			    err);
	}
	sx_xunlock(&c->uq_sx);
	return (left);
}

/* A user-mode queue's fence, as a VA of its VM (QUEUE_MAP2). */
uint64_t
amd_si_uq_fence_va(struct atrium_amd_softc *sc, int slot)
{
	struct amd_si_client *c = sc->si_client;
	uint64_t va = 0;

	if (c == NULL || slot < 0 || slot >= AMD_UQ_SLOTS)
		return (0);
	mtx_lock(&sc->lock);
	if (c->uq[slot].hw != NULL)
		va = amd_mes12_uq_fence_va(c->uq[slot].hw);
	mtx_unlock(&sc->lock);
	return (va);
}

/* A user-mode queue's doorbell page, for its pager (rs/uqueue.rs). */
uint64_t
amd_si_uq_doorbell_pa(struct atrium_amd_softc *sc, int slot)
{
	struct amd_si_client *c = sc->si_client;
	uint64_t pa = 0;

	if (c == NULL || slot < 0 || slot >= AMD_UQ_SLOTS)
		return (0);
	mtx_lock(&sc->lock);
	if (c->uq[slot].hw != NULL && c->uq[slot].user)
		pa = amd_mes12_uq_doorbell_pa(c->uq[slot].hw);
	mtx_unlock(&sc->lock);
	return (pa);
}

/*
 * 1 while the queue in `slot` runs (not lost, the card usable): its pager
 * maps pages only then; a lost queue's next touch is SIGBUS. Called with
 * the pager object locked (order: object, then sc->lock).
 */
int
amd_si_uq_alive(struct atrium_amd_softc *sc, int slot)
{
	struct amd_si_client *c = sc->si_client;
	int alive;

	if (c == NULL || slot < 0 || slot >= AMD_UQ_SLOTS)
		return (0);
	mtx_lock(&sc->lock);
	alive = c->uq[slot].hw != NULL && !c->uq[slot].dead &&
	    amd_si_usable(sc);
	mtx_unlock(&sc->lock);
	return (alive);
}

/* A syncobj is closing: forget it in every user queue's owed list. */
void
amd_si_uq_scrub(struct atrium_amd_softc *sc, struct atrium_amd_syncobj *so)
{
	struct amd_si_client *c = sc->si_client;
	u_int i, j;

	if (!amd_is_si(sc) || c == NULL)
		return;
	mtx_lock(&sc->lock);
	for (i = 0; i < AMD_UQ_SLOTS; i++)
		for (j = 0; j < ATRIUM_AMD_MAX_PENDING; j++)
			if (c->uq[i].owed[j].so == so)
				c->uq[i].owed[j].so = NULL;
	mtx_unlock(&sc->lock);
}

/* ---- the backend vtable ---- */

static int
si_vm_setup(struct atrium_amd_softc *sc, struct atrium_amd_vm *vm)
{
	struct amd_si_client *c = sc->si_client;
	struct amd_vm12 *v;
	uint16_t vmid;
	int err;

	if (c == NULL || !amd_si_usable(sc))
		return (ENXIO);
	mtx_lock(&sc->lock);
	for (vmid = SI_KVMID_FIRST; vmid <= SI_KVMID_LAST; vmid++)
		if ((c->vmids & (1u << vmid)) == 0)
			break;
	if (vmid > SI_KVMID_LAST) {
		mtx_unlock(&sc->lock);
		return (ENOSPC);
	}
	c->vmids |= 1u << vmid;
	sc->vm_count++;
	mtx_unlock(&sc->lock);

	v = malloc(sizeof(*v), M_DEVBUF, M_WAITOK | M_ZERO);
	/* Tables from the VRAM allocator (the whole BO window); the fixed
	 * pool only if the allocator is not up. */
	err = amd_gmc12_vm_init(c->gt, v, sc->vram.inited ? AMD_VM12_4L_HEAP :
	    AMD_VM12_4L_VRAM, 0);
	if (err != 0)
		goto fail;
	if ((err = amd_si_gfx_hold(sc)) != 0)
		goto fail;
	if ((err = amd_gmc12_vm_bind(c->gt, vmid, v)) != 0) {
		(void)amd_gmc12_vm_bind(c->gt, vmid, NULL);
		amd_si_gfx_release(sc);
		goto fail;
	}
	vm->vmid = vmid;
	vm->hw12 = v;
	return (0);
fail:
	if (v->live)
		amd_gmc12_vm_fini(v);
	free(v, M_DEVBUF);
	mtx_lock(&sc->lock);
	c->vmids &= ~(1u << vmid);
	sc->vm_count--;
	mtx_unlock(&sc->lock);
	return (err);
}

static void
si_vm_teardown(struct atrium_amd_vm *vm)
{
	struct atrium_amd_softc *sc = vm->sc;
	struct amd_si_client *c = sc->si_client;
	struct amd_vm12 *v = vm->hw12;

	/* Stop what runs under the VMID, then point it at nothing (queue.rs
	 * calls back into amd_si_q_stop and amd_si_vm_unbind). */
	amd_hwq_vm_gone(sc, vm->vmid);
	if (v != NULL) {
		amd_gmc12_vm_fini(v);
		free(v, M_DEVBUF);
		vm->hw12 = NULL;
	}
	amd_si_gfx_release(sc);
	mtx_lock(&sc->lock);
	c->vmids &= ~(1u << vm->vmid);
	sc->vm_count--;
	mtx_unlock(&sc->lock);
}

static int
si_bo_alloc(struct atrium_amd_softc *sc, struct atrium_amd_bo *bo,
    uint64_t size, uint32_t flags)
{
	/* VRAM only with the allocator up (caps say so; the front-end
	 * refuses first) and a clear engine to scrub what is freed. */
	if ((flags & ATRIUM_GPU_BO_VRAM) != 0 && !sc->vram.inited)
		return (EOPNOTSUPP);
	return (amd_bo_backing_alloc(sc, bo, size, flags));
}

static int
si_map_page(struct atrium_amd_vm *vm, uint64_t va, vm_paddr_t phys, int vram,
    u_int want)
{
	struct amd_si_client *c = vm->sc->si_client;
	struct atrium_amd_vram *v = &vm->sc->vram;
	u_int prot = AMD_VM_READ | (want & (AMD_VM_WRITE | AMD_VM_EXEC));
	int err;

	/*
	 * GPU W^X (ABI 2.3 §5.4): only pages bound EXEC (read-only shader
	 * code) are executable. Until the box has shown which fetches need X
	 * (IBs, a queue's writeback and EOP pages), isolation.gpu_wx = 0
	 * keeps the old mapping, X on every page.
	 */
	if ((atomic_load_int(&vm->sc->gpu_wx) & AMD_WX_BO) == 0)
		prot |= AMD_VM_EXEC;

	if (vm->hw12 == NULL)
		return (EINVAL);
	if (!amd_si_usable(vm->sc))
		return (ENXIO);
	if (vram) {
		/* A VRAM offset from the allocator: never outside its range
		 * (the driver's own area, the PSP's). PTE = FB physical. */
		if (!v->inited || phys < v->lo || phys >= v->lo + v->total ||
		    (phys & PAGE_MASK) != 0)
			return (EINVAL);
		phys += c->gt->vram_base_offset;
		prot |= AMD_VM_VRAM;
	}
	err = amd_gmc12_vm_map(vm->hw12, va, phys, prot);
	/* A walk may have cached the invalid entry this replaces. */
	if (err == 0)
		err = si_vm_flush(c, vm->vmid);
	return (err);
}

static void
si_unmap_page(struct atrium_amd_vm *vm, uint64_t va)
{
	struct amd_si_client *c = vm->sc->si_client;

	if (vm->hw12 == NULL || amd_gmc12_vm_unmap(vm->hw12, va) != 0)
		return;
	/* Before the page can be freed: no stale translation survives. */
	if (amd_si_usable(vm->sc))
		(void)si_vm_flush(c, vm->vmid);
}

static int
si_mmap_single(struct atrium_amd_softc *sc __unused,
    struct atrium_amd_client *client __unused, vm_ooffset_t *offset __unused,
    vm_size_t size __unused, vm_object_t *object __unused, int nprot __unused)
{
	return (EOPNOTSUPP);	/* 5c-2b: user-mode queue doorbells */
}

static void
si_get_caps(struct atrium_amd_softc *sc, struct atrium_gpu_backend_caps *c)
{
	c->vendor = "Atrium AMD (silicon)";
	c->features = ATRIUM_GPU_FEAT_COMPUTE | ATRIUM_GPU_FEAT_SYNCOBJ |
	    ATRIUM_GPU_FEAT_VM_BIND;
	c->va_base = ATRIUM_AMD_BO_VA_BASE;
	c->va_size = (uint64_t)ATRIUM_AMD_VM_MAX_BO * PAGE_SIZE;
	c->va_align = PAGE_SIZE;
	/* The device heap: the allocator's range (5d), if it is up. */
	c->vram_bytes = sc->vram.inited ? sc->vram.total : 0;
	c->bind_features = ATRIUM_GPU_BIND_FEAT_READ_ONLY | ATRIUM_GPU_BIND_FEAT_EXEC;
	c->isa_family = sc->isa_family;
	c->isa_revision = sc->isa_revision;
	c->isa_name = sc->isa_name;
}

const struct atrium_gpu_backend_ops amd_si_backend = {
	.name = "amd-silicon",
	.vm_setup = si_vm_setup,
	.vm_teardown = si_vm_teardown,
	.bo_alloc = si_bo_alloc,
	.bo_free = amd_bo_backing_free,
	.map_page = si_map_page,
	.unmap_page = si_unmap_page,
	.submit = amd_hwq_submit,
	.export_scanout = amd_export_scanout,
	.bo_fget = amd_bo_fget,
	.syncobj_fget = amd_syncobj_fget,
	.syncobj_signal = amd_syncobj_signal,
	.syncobj_fail = amd_syncobj_fail,
	.syncobj_watch = amd_syncobj_watch,
	.syncobj_ready = amd_syncobj_ready,
	.mmap_single = si_mmap_single,
	.get_caps = si_get_caps,
	/* queue_map (5c-2b), gpu_reset, sched, powergate: not yet. */
};

/* ---- VRAM scrub (5d) ---- */

/*
 * The clear engine vram.c calls (its own thread; sleeps). Fills of at most
 * AMD_KQ_FILL_MAX, each waited for: the fence must pass with the queue's
 * generation unchanged, so a reset in between (which starts sequence
 * numbers over) fails the range rather than passing it. A range that fails
 * stays dirty and vram.c retries it later. Only the allocator's range.
 */
static int
si_vram_clear(struct atrium_amd_softc *sc, uint64_t off, uint64_t len)
{
	struct amd_si_client *c = sc->si_client;
	struct atrium_amd_vram *v = &sc->vram;
	uint32_t seq, gen;
	uint64_t n;
	sbintime_t end;
	int err = 0;

	if (c == NULL || c->kq == NULL || !amd_si_usable(sc))
		return (ENXIO);
	if ((off & PAGE_MASK) != 0 || (len & PAGE_MASK) != 0 || len == 0 ||
	    off < v->lo || off + len > v->lo + v->total)
		return (EINVAL);
	if ((err = amd_si_gfx_hold(sc)) != 0)
		return (err);
	while (err == 0 && len > 0) {
		n = MIN(len, AMD_KQ_FILL_MAX);
		end = sbinuptime() + SI_SCRUB_TIMEOUT_MS * SBT_1MS;
		/* Wait for the queue to be idle between client submits. */
		for (;;) {
			mtx_lock(&sc->lock);
			err = c->broken ? EIO : amd_mes12_kq_fill(c->kq,
			    c->gt->fb_start + off, n, &seq, &gen);
			if (err == 0)
				c->scrub_fills++;
			else if (err == EBUSY)
				c->scrub_busy_waits++;
			mtx_unlock(&sc->lock);
			if (err != EBUSY || sbinuptime() >= end)
				break;
			pause_sbt("vramidl", SBT_1MS, 0, 0);
		}
		while (err == 0 && !fence_passed(amd_mes12_kq_fence(c->kq),
		    seq) && amd_mes12_kq_gen(c->kq) == gen) {
			if (sbinuptime() >= end || !amd_si_usable(sc))
				err = ETIMEDOUT;
			else
				pause_sbt("vramscr", SBT_1MS, 0, 0);
		}
		if (err == 0 && amd_mes12_kq_gen(c->kq) != gen)
			err = EIO;	/* reset under it: not known done */
		off += n;
		len -= n;
	}
	amd_si_gfx_release(sc);
	return (err);
}

/*
 * Before the engine is trusted with VRAM: the allocator's first two pages
 * (dirty, not handed out yet) get a pattern through BAR0; a fill of the
 * first must zero it and leave the second alone, which proves the MC
 * address is the offset the allocator meant. Skipped (engine still
 * registered) if BAR0 does not reach that far.
 */
static int
si_vram_selftest(struct atrium_amd_softc *sc, struct amd_si_client *c)
{
	struct resource *bar = c->gt->vram;
	uint64_t off = sc->vram.lo;
	u_int i, zero = 0, kept = 0, words = PAGE_SIZE / 4;
	int err;

	if (bar == NULL || rman_get_size(bar) < off + 2 * PAGE_SIZE) {
		device_printf(sc->dev, "si: VRAM scrub self-test skipped (BAR0 "
		    "%#jx bytes)\n", bar == NULL ? 0 :
		    (uintmax_t)rman_get_size(bar));
		return (0);
	}
	bus_set_region_4(bar, off, 0xa5a5a5a5u, 2 * words);
	amd_hdp_flush(sc);
	if ((err = si_vram_clear(sc, off, PAGE_SIZE)) != 0) {
		device_printf(sc->dev, "si: VRAM scrub self-test: fill failed "
		    "(%d)\n", err);
		return (err);
	}
	for (i = 0; i < words; i++) {
		zero += bus_read_4(bar, off + i * 4) == 0;
		kept += bus_read_4(bar, off + PAGE_SIZE + i * 4) == 0xa5a5a5a5u;
	}
	device_printf(sc->dev, "si: VRAM scrub self-test at %#jx (MC %#jx): "
	    "%u/%u words zeroed, %u/%u of the next page kept: %s\n",
	    (uintmax_t)off, (uintmax_t)(c->gt->fb_start + off), zero, words,
	    kept, words, zero == words && kept == words ? "PASS" : "FAIL");
	return (zero == words && kept == words ? 0 : EIO);
}

/* ---- attach / detach (module.c) ---- */

int
amd_si_client_up(struct atrium_amd_softc *sc)
{
	struct amd_si_client *c;
	struct mes *m = amd_si_mes(sc);
	int err;

	if (!amd_si_usable(sc) || m == NULL || amd_si_ih(sc) == NULL)
		return (ENXIO);
	c = malloc(sizeof(*c), M_DEVBUF, M_WAITOK | M_ZERO);
	c->sc = sc;
	c->gt = amd_si_gart(sc);
	c->ih = amd_si_ih(sc);
	sx_init(&c->uq_sx, "atrium_si_uq");
	callout_init_mtx(&c->uq_poll, &sc->lock, 0);
	TASK_INIT(&c->uq_task, 0, si_uq_evict_task, c);
	if ((err = amd_si_gfx_hold(sc)) != 0) {
		sx_destroy(&c->uq_sx);
		free(c, M_DEVBUF);
		return (err);
	}
	err = amd_mes12_kq_open(m, &c->kq);
	amd_si_gfx_release(sc);
	if (err != 0) {
		sx_destroy(&c->uq_sx);
		free(c, M_DEVBUF);
		return (err);
	}
	amd_si_client_attached(sc, true);
	sc->si_client = c;
	amd_ih7_set_consumer(c->ih, si_iv, c);
	/* 5d: the scrub engine, once it proved where it writes. Without it
	 * freed VRAM (and the dirty start) is never handed out. */
	if (sc->vram.inited) {
		if (si_vram_selftest(sc, c) == 0)
			amd_vram_set_clear(sc, si_vram_clear);
		else
			device_printf(sc->dev, "si: no VRAM scrub engine: VRAM "
			    "stays dirty, VRAM BOs fail with ENOMEM\n");
	}
	return (0);
}

void
amd_si_client_down(struct atrium_amd_softc *sc)
{
	struct amd_si_client *c = sc->si_client;

	if (c == NULL)
		return;
	/* The scrub uses the kernel queue: stop it first (waits out a range
	 * in progress). */
	amd_vram_set_clear(sc, NULL);
	device_printf(sc->dev, "si: VRAM scrub: %ju fills, %ju waits for an "
	    "idle queue\n", (uintmax_t)c->scrub_fills,
	    (uintmax_t)c->scrub_busy_waits);
	amd_ih7_set_consumer(c->ih, NULL, NULL);
	callout_drain(&c->uq_poll);
	taskqueue_drain(taskqueue_thread, &c->uq_task);
	if (c->uq_live != 0)
		device_printf(sc->dev, "si: %u user queue(s) leaked at client "
		    "detach\n", c->uq_live);
	if (amd_si_usable(sc) && amd_si_gfx_hold(sc) == 0) {
		amd_mes12_kq_close(c->kq);
		amd_si_gfx_release(sc);
	} else
		device_printf(sc->dev, "si: card not usable at client "
		    "detach; the queue's block is leaked\n");
	KASSERT(c->vmids == 0, ("si: VMs live at client detach"));
	sc->si_client = NULL;
	amd_si_client_attached(sc, false);
	sx_destroy(&c->uq_sx);
	free(c, M_DEVBUF);
}
