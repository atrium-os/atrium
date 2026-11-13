/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * si_client.c — the client node on silicon (stage 5c-2a,
 * docs/spec/r9700-bare-metal.md §4.5). The Rust front-end (rs/) is the same
 * one gpusim runs under: VM, BO and syncobj fds, SUBMIT, quotas, deferred
 * frees, the hang watchdog. This file is what it reaches on silicon:
 *
 *   VMs     each VM fd gets one of the kernel's VMIDs (1-7; MES owns 8-15)
 *           and its own four-level GPUVM table (gmc12.c, 5b), so a VM's
 *           work can reach only what is bound in it.
 *   BOs     System memory (bus_dma pages, bo.c). VRAM BOs wait for the
 *           silicon VRAM allocator (5d): EOPNOTSUPP.
 *   SUBMIT  the compute engine only. The kernel compute queue (mes12.c,
 *           amd_kq: VMID 0, mapped by MES) runs the client's ring as an
 *           INDIRECT_BUFFER under the VM's VMID, then writes a sequence
 *           number to a fence in VMID 0 and raises an EOP interrupt. The
 *           client cannot write the fence (it is not in its VM), so a
 *           completion is retired only when the fence says so, never on an
 *           interrupt alone.
 *   IRQs    the IH ring's consumer (ih7.c): CP EOP IVs retire signalled
 *           submits by fence; GFX UTCL2 fault IVs (client 0xa, src 0) mark
 *           the queue's current work faulted if it runs under the faulting
 *           VMID, and the watchdog then stops it: the stuck waves of that
 *           VMID are killed (SQ_CMD) and the queue fences on (5b), or is
 *           mapped afresh if it does not.
 *
 * The front-end's queue model (rs/queue.rs) has one queue per engine and
 * reprograms it per submit; on silicon "program" only records the ring and
 * VMID, and the queue reports itself still fetching until the submit's
 * fence lands, so the front-end lets one submit run at a time and holds a
 * BO free until the work that could reach it has fenced.
 *
 * User-mode queues (QUEUE_MAP, doorbell mmap) are 5c-2b: EOPNOTSUPP here.
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

	/* The IV consumer's batch (interrupt thread only). */
	u_int	 b_eop, b_fault, b_sq, b_other;
	uint16_t b_fault_vmid;
	uint64_t b_fault_addr;
	struct timeval log_last;
	int	 log_pps;
};

static bool
fence_passed(uint32_t fence, uint32_t seq)
{
	return ((int32_t)(fence - seq) >= 0);
}

/* ---- the IV consumer (ih7.c), interrupt thread ---- */

static void
si_iv_batch_end(struct amd_si_client *c)
{
	struct atrium_amd_softc *sc = c->sc;
	const u_int q = ATRIUM_AMD_COMPUTE_QID;
	uint32_t fence, eop = 0, status;
	uint64_t addr;
	bool fault = false, log;
	int i;

	log = ppsratecheck(&c->log_last, &c->log_pps, 5);
	mtx_lock(&sc->lock);
	for (i = 0; i < ATRIUM_AMD_NHWQ; i++) {
		sc->ih_eop_ring[i] = 0;
		sc->ih_fault_ring[i] = 0;
	}
	if (c->b_fault != 0) {
		sc->iso.vm_faults += c->b_fault;
		/* Re-arm the latch (GC registers: valid while VMs hold
		 * GfxOff off, and only a VM's work can fault here). */
		if (c->vmids != 0 && amd_gmc12_fault_take(c->gt, &status,
		    &addr) && log)
			amd_gmc12_fault_decode(c->gt, status, "client fault");
		if (c->b_fault_vmid != 0 && c->b_fault_vmid == c->last_vmid &&
		    c->kq != NULL && !fence_passed(amd_mes12_kq_fence(c->kq),
		    c->last_seq)) {
			sc->ih_fault_ring[q] = c->b_fault;
			sc->ih_fault_vmid[q] = c->b_fault_vmid;
			sc->ih_fault_page[q] = (uint32_t)(c->b_fault_addr >> 12);
			fault = true;
		} else if (log)
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
	sc->ih_eop_ring[q] = eop;
	/* The fault first, so the completions it covers fail (EIO). */
	if (fault && sc->ih_handler[ATRIUM_AMD_IH_CAUSE_VM_FAULT] != NULL)
		sc->ih_handler[ATRIUM_AMD_IH_CAUSE_VM_FAULT](sc, c->b_fault);
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
	c->b_eop = c->b_fault = c->b_sq = c->b_other = 0;
	c->b_fault_vmid = 0;
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
	if (client == IH_CLIENT_GRBM_CP && src == IH_SRC_CP_EOP)
		c->b_eop++;
	else if (client == IH_CLIENT_GFX && src == IH_SRC_UTCL2_FAULT) {
		/* gmc_v12_0_process_interrupt: VMID in dword 0, the page in
		 * data 0 and bits 47:44 in data 1. */
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
	err = amd_gmc12_vm_init(c->gt, v, AMD_VM12_4L_VRAM, 0);
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
	if ((flags & ATRIUM_GPU_BO_VRAM) != 0)
		return (EOPNOTSUPP);	/* 5d: the silicon VRAM allocator */
	return (amd_bo_backing_alloc(sc, bo, size, flags));
}

static int
si_map_page(struct atrium_amd_vm *vm, uint64_t va, vm_paddr_t phys, int vram,
    int writable)
{
	struct amd_si_client *c = vm->sc->si_client;
	int err;

	if (vram || vm->hw12 == NULL)
		return (EINVAL);
	if (!amd_si_usable(vm->sc))
		return (ENXIO);
	err = amd_gmc12_vm_map(vm->hw12, va, phys, AMD_VM_READ | AMD_VM_EXEC |
	    (writable ? AMD_VM_WRITE : 0));
	/* A walk may have cached the invalid entry this replaces. */
	if (err == 0)
		err = amd_gmc12_vm_flush(c->gt, vm->vmid);
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
		(void)amd_gmc12_vm_flush(c->gt, vm->vmid);
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
	c->vram_bytes = 0;		/* no VRAM BOs before 5d */
	c->bind_features = ATRIUM_GPU_BIND_FEAT_READ_ONLY;
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
	if ((err = amd_si_gfx_hold(sc)) != 0) {
		free(c, M_DEVBUF);
		return (err);
	}
	err = amd_mes12_kq_open(m, &c->kq);
	amd_si_gfx_release(sc);
	if (err != 0) {
		free(c, M_DEVBUF);
		return (err);
	}
	amd_si_client_attached(sc, true);
	sc->si_client = c;
	amd_ih7_set_consumer(c->ih, si_iv, c);
	return (0);
}

void
amd_si_client_down(struct atrium_amd_softc *sc)
{
	struct amd_si_client *c = sc->si_client;

	if (c == NULL)
		return;
	amd_ih7_set_consumer(c->ih, NULL, NULL);
	if (amd_si_usable(sc) && amd_si_gfx_hold(sc) == 0) {
		amd_mes12_kq_close(c->kq);
		amd_si_gfx_release(sc);
	} else
		device_printf(sc->dev, "si: card not usable at client "
		    "detach; the queue's block is leaked\n");
	KASSERT(c->vmids == 0, ("si: VMs live at client detach"));
	sc->si_client = NULL;
	amd_si_client_attached(sc, false);
	free(c, M_DEVBUF);
}
