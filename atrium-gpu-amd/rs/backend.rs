/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * backend.rs — the one place the front-end calls into C.
 *
 * `Dev` is a device (its C softc) seen from the front-end: the backend vtable
 * (`sc->backend`, the per-architecture hardware seam: amd = GPUVM + PM4 over
 * gpusim or silicon) and the few C helpers the front-end needs from queue.c,
 * irq.c and glue.c. Each call here is an FFI call and so `unsafe` inside; the
 * methods are safe because their arguments are live objects the front-end
 * owns, and because the C side honours the contracts written next to each op
 * in atrium_gpu_amd.h. That trust in the C backend is where the port's safety
 * ends, by design: the hardware half stays C for now.
 */

use crate::defer::{BusyAt, DevState};
use crate::sys;
use core::ffi::{c_int, c_void};
use kpi::prelude::*;
use kpi::ErrCode;

/// A GPU device. The softc outlives every object made from it: module detach waits until the
/// BO, VM and syncobj counts are zero and the cdevs are destroyed.
#[derive(Copy, Clone)]
pub struct Dev(*mut sys::atrium_amd_softc);

// SAFETY: the softc is shared device state with its own locks.
unsafe impl Send for Dev {}
unsafe impl Sync for Dev {}

fn check(err: c_int) -> Result<()> {
    if err == 0 { Ok(()) } else { Err(ErrCode::from(err)) }
}

impl Dev {
    /// # Safety
    ///
    /// `sc` is an attached device's softc (a cdev's si_drv1).
    pub unsafe fn from_raw(sc: *mut sys::atrium_amd_softc) -> Self {
        Dev(sc)
    }

    /// The device behind one of its cdevs.
    ///
    /// # Safety
    ///
    /// `dev` is a cdev this module created (si_drv1 is the softc).
    pub unsafe fn from_cdev(dev: *mut kpi::bindings::cdev) -> Self {
        Dev(unsafe { sys::amd_rs_cdev_softc(dev) })
    }

    pub fn raw(self) -> *mut sys::atrium_amd_softc {
        self.0
    }

    fn ops(self) -> &'static sys::atrium_gpu_backend_ops {
        // SAFETY: a static vtable of this module, set before any cdev exists.
        unsafe { &*sys::amd_rs_backend(self.0) }
    }

    // --- address spaces (vm_setup self-cleans on failure; teardown undoes a setup) ---

    pub fn vm_setup(self, vm: &mut sys::atrium_amd_vm) -> Result<()> {
        check(unsafe { (self.ops().vm_setup.unwrap())(self.0, vm) })
    }

    pub fn vm_teardown(self, vm: &mut sys::atrium_amd_vm) {
        unsafe { (self.ops().vm_teardown.unwrap())(vm) }
    }

    /// Write one page's PTE in `vm`. The VA lies in the VM's window (the arena handed it out).
    pub fn map_page(self, vm: *mut sys::atrium_amd_vm, va: u64, phys: u64, vram: bool, writable: bool) -> Result<()> {
        check(unsafe { (self.ops().map_page.unwrap())(vm, va, phys as _, vram as c_int, writable as c_int) })
    }

    pub fn unmap_page(self, vm: *mut sys::atrium_amd_vm, va: u64) {
        unsafe { (self.ops().unmap_page.unwrap())(vm, va) }
    }

    // --- buffer objects ---

    /// Allocate the backing store into `bo` (self-cleaning on failure).
    pub fn bo_alloc(self, bo: &mut sys::atrium_amd_bo, size: u64, flags: u32) -> Result<()> {
        check(unsafe { (self.ops().bo_alloc.unwrap())(self.0, bo, size, flags) })
    }

    /// Release the backing store. Only once no queue can still reach it.
    pub fn bo_free(self, bo: &mut sys::atrium_amd_bo) {
        unsafe { (self.ops().bo_free.unwrap())(bo) }
    }

    /// Which hardware queues may still be running work under any of `vmids`, and each queue's
    /// generation now (queue.rs). Busy queues hold up a BO's free.
    pub fn busy_snapshot(self, vmids: &[u16]) -> BusyAt {
        self.state().queues.snapshot(self, Some(vmids))
    }

    /// Which queues are busy now (under any VM), and each queue's generation.
    pub fn queues_now(self) -> BusyAt {
        self.state().queues.snapshot(self, None)
    }

    /// The front-end's per-device state.
    pub fn state(self) -> &'static DevState {
        self.try_state().expect("rs_state set at attach")
    }

    /// The per-device state, if attach has set it up (and detach not yet torn it down).
    pub fn try_state(self) -> Option<&'static DevState> {
        let p = unsafe { sys::amd_rs_state(self.0) };
        // SAFETY: set at attach (amd_hwq_init) and freed at detach after every object made
        // from this device is gone.
        (!p.is_null()).then(|| unsafe { DevState::from_raw(p) })
    }

    /// Account deferred frees: `now` more (or fewer) waiting, `reaped` more completed.
    pub fn defer_count(self, now: c_int, reaped: c_int) {
        unsafe { sys::amd_rs_defer_count(self.0, now, reaped) }
    }

    /// Make sure the reaper runs soon.
    pub fn defer_kick(self) {
        unsafe { sys::amd_hwq_defer_kick(self.0) }
    }

    // --- submission (the backend op queue.rs implements for amd) ---

    #[allow(clippy::too_many_arguments)]
    pub fn submit(
        self,
        client: *mut sys::atrium_amd_client,
        ring_va: u64,
        n_dwords: u32,
        engine: u32,
        vm: *mut sys::atrium_amd_vm,
        deadline_ns: u32,
        so: *mut sys::atrium_amd_syncobj,
        signal_value: u64,
    ) -> Result<()> {
        check(unsafe {
            (self.ops().submit.unwrap())(self.0, client, ring_va, n_dwords, engine, vm, deadline_ns, so, signal_value)
        })
    }

    /// Make `client` the exclusive owner of the queue for `engine`, running the ring at
    /// `ring_va` in `vm`. Returns the doorbell's page offset. `EOPNOTSUPP` without user queues.
    pub fn queue_map(self, client: *mut sys::atrium_amd_client, vm: *mut sys::atrium_amd_vm, ring_va: u64, engine: u32) -> Result<u32> {
        let f = self.ops().queue_map.ok_or(EOPNOTSUPP)?;
        let mut off = 0u32;
        check(unsafe { f(self.0, client, vm, ring_va, engine, &mut off) })?;
        Ok(off)
    }

    /// The doorbell mmap (queue.rs): checks the offset belongs to a queue `client` owns and
    /// returns a revocable pager object.
    ///
    /// # Safety
    ///
    /// The arguments are d_mmap_single's.
    pub unsafe fn mmap_single(
        self,
        client: *mut sys::atrium_amd_client,
        offset: *mut kpi::bindings::vm_ooffset_t,
        size: kpi::bindings::vm_size_t,
        object: *mut kpi::bindings::vm_object_t,
        nprot: c_int,
    ) -> c_int {
        unsafe { (self.ops().mmap_single.unwrap())(self.0, client, offset, size, object, nprot) }
    }

    /// Give back any queue `client` owns and revoke its doorbell mappings (queue.rs).
    pub fn client_close(self, client: *mut sys::atrium_amd_client) {
        unsafe { crate::queue::amd_hwq_client_close(self.0, client) }
    }

    // --- queue hardware and the completions a queue owes (queue.c helpers, cp.c) ---

    /// Program `engine`'s queue to run the ring at `ring_va` under `vmid`; the doorbell offset.
    pub fn queue_program(self, ring_va: u64, engine: u32, vmid: u16) -> Result<u32> {
        let mut off = 0u32;
        check(unsafe { sys::amd_queue_program(self.0, ring_va, engine, vmid, &mut off) })?;
        Ok(off)
    }

    /// Halt queue `qid`, dropping what it had queued.
    pub fn queue_stop(self, qid: u32) {
        unsafe { sys::amd_queue_stop(self.0, qid) }
    }

    /// The queue's read pointer.
    pub fn q_rptr(self, qid: u32) -> u32 {
        unsafe { sys::amd_rs_q_rptr(self.0, qid) }
    }

    /// Does the queue still owe a completion (an EOP not yet seen)?
    pub fn q_owes(self, qid: u32) -> bool {
        crate::pending::owes(self, qid)
    }

    /// Is there room on the queue's list for one more completion?
    pub fn q_room(self, qid: u32) -> bool {
        crate::pending::has_room(self, qid)
    }

    /// New work was programmed: register its completion (if `so`) and restart the hang clock.
    pub fn q_started(self, qid: u32, so: *mut sys::atrium_amd_syncobj, value: u64) {
        crate::pending::started(self, qid, so, value)
    }

    /// The queue was stopped: fail what it owed (EIO).
    pub fn q_stopped(self, qid: u32, was_busy: bool) {
        crate::pending::stopped(self, qid, was_busy)
    }

    /// Must this busy kernel-rung queue be stopped (it faulted, or made no progress for
    /// hang_ms)? Logs and counts the reason.
    pub fn q_hang(self, qid: u32, vmid: u16, rptr: u32) -> bool {
        crate::pending::hung(self, qid, vmid, rptr)
    }

    // --- sc->lock, for pending.rs's ScLocked ---

    pub fn sc_mtx_lock(self) {
        unsafe { sys::amd_rs_sc_lock(self.0) }
    }

    pub fn sc_mtx_unlock(self) {
        unsafe { sys::amd_rs_sc_unlock(self.0) }
    }

    pub fn sc_mtx_assert(self) {
        unsafe { sys::amd_rs_sc_assert(self.0) }
    }

    /// The kernel's tick counter (wraps; compare with wrapping_sub).
    pub fn ticks(self) -> c_int {
        unsafe { sys::amd_rs_ticks() }
    }

    /// hang_ms in ticks. sc->lock held.
    pub fn hang_ticks_locked(self) -> c_int {
        unsafe { sys::amd_rs_hang_ticks_locked(self.0) }
    }

    /// Add `n` to an isolation counter (AMD_RS_CNT_*). sc->lock held.
    pub fn count_locked(self, what: u32, n: u32) {
        unsafe { sys::amd_rs_count_locked(self.0, what as c_int, n) }
    }

    /// Log why the hang check stopped a queue (AMD_RS_STOP_*). Without sc->lock.
    pub fn q_report_stop(self, qid: u32, vmid: u16, what: u32) {
        unsafe { sys::amd_rs_q_report_stop(self.0, qid, vmid, what as c_int) }
    }

    pub fn q_count(self, what: u32) {
        unsafe { sys::amd_rs_q_count(self.0, what as c_int) }
    }

    pub fn q_deadline(self, ns: u32) {
        unsafe { sys::amd_rs_q_deadline(self.0, ns) }
    }

    /// Ring the doorbell. `signalled`: the submit put a completion on the queue's list, so
    /// silicon counts its fence against that list (gpusim ignores it).
    pub fn q_doorbell(self, off: u32, wptr: u32, signalled: bool) {
        unsafe { sys::amd_rs_q_doorbell(self.0, off, wptr, signalled as c_int) }
    }

    /// The doorbell BAR's base physical address, 0 if it cannot be mapped.
    pub fn doorbell_pa(self) -> u64 {
        unsafe { sys::amd_rs_doorbell_pa(self.0) }
    }

    /// Point `vmid` at the all-invalid page directory.
    pub fn vm_null_pd(self, vmid: u16) {
        unsafe { sys::amd_rs_vm_null_pd(self.0, vmid) }
    }

    /// Make sure the hang watchdog is running (there is kernel-rung work).
    pub fn watchdog_arm(self) {
        unsafe { sys::amd_rs_watchdog_arm(self.0) }
    }

    // --- syncobjs ---

    /// Forget every completion the ISR still owes `so` (it is about to be freed). Takes
    /// sc->lock, so the ISR is not using `so` once this returns.
    pub fn pending_scrub(self, so: *mut sys::atrium_amd_syncobj) {
        crate::pending::scrub(self, so)
    }

    pub fn syncobj_count(self, delta: c_int) {
        unsafe { sys::amd_rs_syncobj_count(self.0, delta) }
    }

    // --- doorbell grants ---

    /// Account a doorbell grant: `live` grant objects more (or fewer), and one more created.
    /// Live grants hold up module detach (amd_hwq_busy_refs).
    /// The calling thread's real uid: what its BOs are charged to (quota.rs).
    pub fn cur_ruid(self) -> u32 {
        unsafe { sys::amd_rs_cur_ruid() }
    }

    /// The per-uid cap in bytes, for VRAM or for System BOs.
    pub fn quota_limit(self, vram: bool) -> u64 {
        unsafe { sys::amd_rs_quota_limit(self.0, vram as c_int) }
    }

    pub fn quota_refused(self) {
        unsafe { sys::amd_rs_quota_refused(self.0) }
    }

    pub fn grant_count(self, live: c_int, created: bool) {
        unsafe { sys::amd_rs_grant_count(self.0, live, created as c_int) }
    }

    // --- device-wide ---

    pub fn caps(self) -> sys::atrium_gpu_backend_caps {
        // SAFETY: integers and string pointers; zero is "absent" for each.
        let mut c: sys::atrium_gpu_backend_caps = unsafe { core::mem::zeroed() };
        unsafe { (self.ops().get_caps.unwrap())(self.0, &mut c) };
        c
    }

    /// Interrupts serviced so far, and whether MSI-X is on.
    pub fn irqs(self) -> (u64, u32) {
        let (mut n, mut msix) = (0u64, 0u32);
        unsafe { sys::amd_rs_irqs(self.0, &mut n, &mut msix) };
        (n, msix)
    }

    pub fn gpu_reset(self) -> Result<()> {
        let f = self.ops().gpu_reset.ok_or(EOPNOTSUPP)?;
        check(unsafe { f(self.0) })
    }

    pub fn sched(self, s: &mut sys::atrium_gpu_sched) -> Result<()> {
        let f = self.ops().sched.ok_or(EOPNOTSUPP)?;
        unsafe { f(self.0, s) };
        Ok(())
    }

    pub fn powergate(self, p: &mut sys::atrium_gpu_powergate) -> Result<()> {
        let f = self.ops().powergate.ok_or(EOPNOTSUPP)?;
        check(unsafe { f(self.0, p) })
    }
}

/// A watcher callback C code installs on a syncobj.
pub type WatchFn = unsafe extern "C" fn(*mut c_void);
