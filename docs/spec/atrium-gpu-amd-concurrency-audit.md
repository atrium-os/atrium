# atrium-gpu-amd concurrency audit (2026-11-13)

Status: F0 fixed (de56be23); F1 and F2 fixed on main, not yet run on the
box; F3-F6 open. Asked for by Shankar after the
G5 wedge turned out to be two threads driving one TLB invalidation engine
(r9700-bare-metal.md §5, 13:53 run). Read from the code at main f16ba0b0; no
lock-order checker (WITNESS) has run on this driver, since the box kernel is
ATRIUM-NODEBUG.

## Who runs driver code at the same time

| Context | Holds | Touches hardware through |
|---|---|---|
| Client ioctls (any number of threads) | `uq_sx` for queue create/destroy/preempt only; nothing for VM map/unmap, submit, kq paths | gmc12 flush, MES API, doorbells, GRBM-selected CP regs, RLC safe mode |
| IH interrupt thread and its poll callout (`ih7_poll`) | `sc->lock` around IV processing | fault status (hub regs), HQD scans (`si_uq_faulted` → `amd_mes12_uq_hqd_vmid` → `uq2_hqd_at`) |
| `uq_poll` callout | `sc->lock` (callout_init_mtx) | hang check: `amd_mes12_uq_hqd_where`, `uq_user_busy` (HQD scans) |
| Eviction task (`taskqueue_thread`) | `uq_sx` (x) | SQ kill (RLC safe mode), HQD scan/reset, MES REMOVE/RESET, pipe reset, flushes, `si_recover` → reinit |
| `si_watch` (`taskqueue_thread`) | `si->lock` (x) | SMU telemetry; mode 1 reset on the busy/hot strike |
| VRAM scrub task (its own taskqueue) | `ce->lock` | kernel queue submits (DMA_DATA) |
| Reinit (`amd_si_reinit`, from `si_recover`) | `si->lock` (x), plus `uq_sx` from its callers | everything: PSP, SMU, GART, IH, MES |

`taskqueue_thread` is one thread, so the eviction task and `si_watch` never
overlap each other. Everything else can overlap.

## Findings, most likely to bite first

**F0. TLB invalidation engine 17 had no lock (fixed, de56be23).** GFXHUB has no
semaphore; the eviction flush and a BO-free flush interleaved and the engine
never acked again. Now one mutex across `amd_gmc12_flush` and
`amd_gmc12_vm_flush`, as amdgpu's `gmc.invalidate_lock`. Awaiting the box run.

**F1. GRBM_GFX_CNTL select-then-access is unserialised (fixed on main:
`amd_grbm_select_write` in gfx12.c, one mutex from a non-zero select to the
select back to 0).** `grbm_select`
(gfx12.c:94), `grbm_select`/`grbm_select_q` (mes12.c:322, 1933) write the
me/pipe/queue/VMID index, and the following reads and writes of the CP_HQD_*,
SH_MEM_* and MEC registers go to whatever that index says *now*. Callers run in:
the IH thread under `sc->lock` (`si_uq_faulted`, `si_uq_cperr` scan all 32 HQDs
on every fault), the `uq_poll` callout, the eviction task (`uq2_hqd_find`,
`uq2_hqd_reset`, `uq2_pipe_reset`), queue create (`uq_hqd_scan`, `amd_mes12_uq_ints`)
and mes_report. A fault IV landing while the eviction task resets an HQD can
make the reset write another queue's HQD, or the scan return the wrong VMID.
This can also explain wave-kill snapshots that differ run to run. amdgpu holds
`srbm_mutex` here.
Fix: one mutex for the select window (select, access, select 0, unlock), taken
inside `sc->lock` where that is held (order: `sc->lock` → grbm). Better still,
remove HQD scans from the interrupt path: look a queue's HQD up once in the task,
not per IV.

**F2. RLC safe mode is not reentrant (fixed on main: depth count under a
mutex in gfx12_pm.c).** `safe_mode_enter`/`exit`
(gfx12_pm.c:148) are a raw handshake. Users: `amd_gfx12_sq_kill_vmid` (the
eviction task, and `amd_si_q_stop`, si_client.c:891, from a client close),
the MEC pipe reset, clock gating. If two overlap, the first `exit` drops safe
mode under the second. amdgpu keeps a depth count under a lock.
Fix: a mutex plus a depth count in gfx12_pm.c.

**F3. A reset or re-init can run under threads that are touching the card.**
`si_watch`'s mode 1 reset and `amd_si_reinit` hold `si->lock` exclusively. But
the client paths that do MMIO and DMA (submit, VM map/unmap and flush, preempt,
kernel-queue submits, the scrub) only test `amd_si_usable()` first, a TOCTOU
check, and never take `si->lock`. A submit's doorbell write, or a flush, can
land mid-reset.
Fix: the client's hardware-touching paths hold `si->lock` shared (or a
generation rwlock) and re-check usability under it; reset/reinit take it
exclusive. Lock order: `si->lock` → `uq_sx` → `sc->lock` → grbm / inv / MES.

**F4. A wedged hub turns into a 20 s storm instead of a recovery.** Each
`hub_flush` waits 100 ms. After the first timeout every later flush times out
too (150–190 per run, about 20 s, much of it under `uq_sx`, now also under the
invalidation mutex). Not a livelock, but every client stalls behind it.
Fix: on the first GFXHUB timeout, mark the hub wedged, fail later flushes at once
with EIO, and hand the card to `si_recover` (reset + reinit, blame the uid).

**F5. The MES API is serialised by convention only.** `mes_api` (mes12.c:682)
shares one ring, `r->seq` and one status slot per pipe. Today all
user-queue callers hold `uq_sx` and init/reinit holds `si->lock`, so no overlap
was found. Nothing enforces it.
Fix: a mutex in `mes_api` (amdgpu's `mes.ring_lock`), or at least
`sx_assert(uq_sx)` on the user-queue entry points.

**F6. Kernel queue submitters (not verified).** The scrub (`ce->lock`), the
kernel-queue stop/reset in si_client.c:885-910, and the legacy SUBMIT path (off)
all write the same ring and wptr. Check that one lock covers every submitter.

**F7. The driver GRBM-selects MES-owned queues and pipes while MES runs
(found on the box, fixed for the fault path).** GRBM_GFX_CNTL selects for
HQD scans, MES-pipe reads and the MEC interrupt enables, made while MES/CP
was busy right after a fault, left CPF's GRBM path and the GFXHUB L2 stuck
(CPF bit 28, GCVM_L2_STATUS 0x1, no invalidation acked): the G5 wedge
(r9700-bare-metal.md §5, runs 8f3d4517 to f4af2b81). F1's lock does not
help: it orders the driver's threads, not the driver against MES firmware.
Fixed: the fault and eviction path find a queue's VMID by the hub's context
page-table base and never select (f4af2b81, 2 of 2 clean full suites); the
MEC1 interrupt enables are set once at MES start (d9391a26). Still selecting
at run time: CP-error IVs (si_uq_cperr maps pipe/queue to a queue by a scan)
and the HQD reset of a queue that will not stop (followed by a mode 1 reset
if it fails). Rule: no GRBM select of a MES-owned queue or pipe while MES
runs, except on a path that ends in a reset.

## Deadlocks and sleeping with locks held

- The new invalidation mutex is MTX_DEF and is never taken under a spin lock.
  `sc->lock` is MTX_DEF, so taking it inside is allowed; no caller was found
  that flushes with `sc->lock` held.
- `sc->lock` is held across 32-HQD register scans in the IH path (F1). That is
  slow but not sleeping.
- Callers that `DELAY()` for up to 100 ms hold `uq_sx` (sleepable sx), which is
  legal but stalls everyone behind it (F4).
- `taskqueue_drain(taskqueue_thread, &c->uq_task)` at detach must not run with
  `uq_sx` held. It doesn't today.

## How to check, not just read

1. Boot the gpusim VM with a WITNESS + INVARIANTS kernel (standing OK) and run
   the acceptance suites. That catches lock-order reversals and sleeping with a
   mutex held. Only the gpusim paths get exercised, though.
2. Add si-client-test 16, a concurrency stress: N threads each binding and
   unbinding BOs, submitting, and faulting their own queue, with a bystander
   checking. It runs for a minute and should be part of the 24 h fuzz gate
   anyway.
3. Fix order: F1 and F2 (cheap, likely), then F4, then F3 (the larger change),
   with F5 and F6 as asserts.
