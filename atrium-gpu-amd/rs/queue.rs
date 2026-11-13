/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * queue.rs — who may use the device's hardware queues, and when (GPU
 * isolation breaks 1 and 3, review §A2.2/§A2.6). Was the queue half of
 * queue.c.
 *
 *   break 1  A queue is reprogrammed only under its lock, and only once its
 *            read pointer has caught up with what was rung: a submitter waits
 *            up to ATRIUM_AMD_QUEUE_WAIT_MS for another VM's work to drain,
 *            then gets EBUSY.
 *   break 3  QUEUE_MAP makes the caller the queue's exclusive owner until it
 *            closes the device or the VM goes away. Only the owner can mmap
 *            the queue's doorbell (doorbell.rs), and the grant is revoked when
 *            ownership ends.
 *   A2.6     A kernel-rung queue that shows no progress for hang_ms is stopped
 *            (the watchdog, or a submitter waiting on it); what it owed fails
 *            with EIO.
 *
 * Each queue's state lives behind its own sx lock (`QState`), so it can only
 * be read or changed with the lock held. What stays C (queue.c helpers, irq.c):
 * register access, programming and stopping a queue (cp.c), and the list of
 * completions a queue owes, which the interrupt handler drains under sc->lock.
 *
 * Lock order: a queue's lock, then a grant object's lock or the deferred
 * list's lock, then sc->lock (inside the C helpers). Queue locks are taken in
 * index order when more than one is held (reset).
 */

use crate::backend::Dev;
use crate::defer::{self, BusyAt};
use crate::doorbell::{self, GrantObj};
use crate::sys;
use core::ffi::c_int;
use core::pin::Pin;
use kpi::bindings::{self, vm_object_t, vm_ooffset_t, vm_size_t};
use kpi::kobj::AsCType;
use kpi::ErrCode;
use kpi::prelude::*;
use kpi::sync::mtx::{SBT_1MS, sbinuptime};
use kpi::sync::sx::{SxExclusiveGuard, SxLock, sx_init_flags, sx_slock, sx_try_xlock, sx_xlock};

pub const NHWQ: usize = sys::ATRIUM_AMD_NHWQ as usize;

/// Who owns a user-mode queue: an open of the client node, in one VM. Identities only, never
/// dereferenced; ownership ends (dequeue) before either object is freed: the client's close
/// and the VM's teardown both release the queue first.
#[derive(Copy, Clone, PartialEq, Eq)]
struct Owner {
    client: usize,
    vm: usize,
}

/// One hardware queue's state. Only reachable through its lock.
struct QState {
    /// VMID programmed now, 0 = none.
    vmid: u16,
    /// Last write pointer the kernel rang, 0 = none (or a user-mode queue).
    wptr: u32,
    /// Bumped by every program and dequeue; the deferred-free list compares it.
    generation: u64,
    owner: Option<Owner>,
    /// The owner's doorbell grant (a reference to its pager object), if mapped.
    grant: Option<GrantObj>,
}

pub struct HwQ {
    qid: u32,
    engine: u32,
    doorbell_off: u32,
    st: SxLock<QState>,
}

type Guard<'a> = SxExclusiveGuard<'a, QState>;

impl HwQ {
    fn new(qid: u32) -> Self {
        let gfx = qid == sys::ATRIUM_AMD_GFX_QID as u32;
        HwQ {
            qid,
            engine: if gfx { sys::ATRIUM_GPU_ENGINE_GFX as u32 } else { sys::ATRIUM_GPU_ENGINE_COMPUTE as u32 },
            doorbell_off: if gfx { sys::ATRIUM_AMD_GFX_DOORBELL as u32 } else { sys::ATRIUM_AMD_COMPUTE_DOORBELL as u32 },
            st: SxLock::new(QState { vmid: 0, wptr: 0, generation: 0, owner: None, grant: None }),
        }
    }

    /// # Safety
    ///
    /// `self` is at its final address.
    unsafe fn init(&self) {
        // DUPOK: reset holds them all, always in index order.
        unsafe { sx_init_flags(Pin::new_unchecked(&self.st), c"atrium_amd_hwq", bindings::SX_DUPOK as c_int) };
    }

    /// Has the CP not yet fetched everything the kernel rang?
    fn fetching(&self, dev: Dev, st: &QState) -> bool {
        st.vmid != 0 && st.wptr != 0 && dev.q_rptr(self.qid) < st.wptr
    }

    /// Could the queue still be executing (or about to execute) work? A queue a client owns
    /// always is: user space rings it, so the kernel does not know its write pointer. A
    /// kernel-rung queue is busy while the CP trails what was rung, or while it owes an EOP
    /// (the CP can be past the last packet with shaders still running).
    fn busy(&self, dev: Dev, st: &QState) -> bool {
        st.owner.is_some() || self.fetching(dev, st) || dev.q_owes(self.qid)
    }

    /// May the queue be reprogrammed for `vmid` now? Never while the CP is still fetching.
    /// The same VM's owed work retires in order behind more of its own; another VM waits
    /// for the queue to be fully idle.
    fn blocks(&self, dev: Dev, st: &QState, vmid: u16) -> bool {
        self.fetching(dev, st) || (st.vmid != vmid && dev.q_owes(self.qid))
    }

    /// Take the doorbell page back from every mapping of the owner's grant.
    fn revoke(&self, dev: Dev, st: &mut QState) {
        if let Some(g) = st.grant.take() {
            doorbell::revoke(g);
            dev.q_count(sys::AMD_RS_Q_REVOKE);
        }
    }

    /// Stop the queue and forget its programming: halt it (dropping queued work), revoke the
    /// owner's doorbell, release ownership, and fail what it owed.
    fn dequeue(&self, dev: Dev, st: &mut QState) {
        let busy = self.busy(dev, st);
        self.revoke(dev, st);
        if st.vmid != 0 {
            dev.queue_stop(self.qid);
        }
        st.owner = None;
        st.vmid = 0;
        st.wptr = 0;
        st.generation += 1;
        dev.q_stopped(self.qid, busy);
    }

    /// The hang check: a busy kernel-rung queue that faulted, or showed no progress for
    /// hang_ms, is stopped. Run by the watchdog and by a submitter waiting on the queue (which
    /// holds the lock the watchdog would need). True if the queue was stopped.
    fn check_hang(&self, dev: Dev, st: &mut QState) -> bool {
        if st.owner.is_some() || !self.busy(dev, st) {
            return false;
        }
        let rptr = if st.vmid != 0 { dev.q_rptr(self.qid) } else { 0 };
        if dev.q_hang(self.qid, st.vmid, rptr) {
            self.dequeue(dev, st);
            return true;
        }
        false
    }

    /// Wait for a kernel-rung queue to drain. Holding the queue's lock while we sleep is the
    /// point: no one else may program or ring it meanwhile. Bounded by the clock, not by
    /// counting pauses: a pause runs long on a loaded or emulated machine.
    fn wait_idle(&self, dev: Dev, st: &mut QState, vmid: u16) -> Result<()> {
        let end = sbinuptime() + sys::ATRIUM_AMD_QUEUE_WAIT_MS as i64 * SBT_1MS;
        let mut waited = false;
        let mut n = 0u32;
        while self.blocks(dev, st, vmid) {
            if n % 10 == 0 && self.check_hang(dev, st) {
                break;
            }
            if sbinuptime() >= end {
                return Err(EBUSY);
            }
            if !waited {
                waited = true;
                dev.q_count(sys::AMD_RS_Q_WAIT);
            }
            unsafe { bindings::pause_sbt(c"amdqidl".as_ptr(), SBT_1MS, 0, 0) };
            n += 1;
        }
        Ok(())
    }
}

/// The queues of one device (in the front-end's per-device state).
pub struct Queues([HwQ; NHWQ]);

impl Queues {
    pub fn new() -> Self {
        Queues(core::array::from_fn(|i| HwQ::new(i as u32)))
    }

    /// # Safety
    ///
    /// `self` is at its final address.
    pub unsafe fn init(&self) {
        for q in &self.0 {
            unsafe { q.init() };
        }
    }

    fn for_engine(&self, engine: u32) -> Option<&HwQ> {
        self.0.iter().find(|q| q.engine == engine)
    }

    /// Which queues are busy under any of `vmids` (all VMs if None), and their generations.
    pub fn snapshot(&self, dev: Dev, vmids: Option<&[u16]>) -> BusyAt {
        let mut at = BusyAt { mask: 0, gens: [0; NHWQ] };
        for (i, q) in self.0.iter().enumerate() {
            let st = sx_slock(&q.st);
            at.gens[i] = st.generation;
            let mine = match vmids {
                None => true,
                Some(v) => st.vmid != 0 && v.contains(&st.vmid),
            };
            if mine && q.busy(dev, &st) {
                at.mask |= 1 << i;
            }
        }
        at
    }
}

fn queues<'a>(dev: Dev) -> &'a Queues {
    &dev.state().queues
}

/// Run the deferred-free pass, and again later while BOs wait.
fn reap(dev: Dev) {
    if defer::reap(dev) {
        dev.defer_kick();
    }
}

fn owner(client: *mut sys::atrium_amd_client, vm: *mut sys::atrium_amd_vm) -> Owner {
    Owner { client: client as usize, vm: vm as usize }
}

fn refused(dev: Dev, e: ErrCode) -> c_int {
    dev.q_count(sys::AMD_RS_Q_REFUSED);
    e.as_c_type()
}

// --- the backend ops and C entry points (module.c, vm.c, queue.c) ---

/// Backend submit: run `n_dwords` of the ring at `ring_va` in `vm` on `engine`'s queue. The
/// front-end has checked the arguments (ioctl.rs).
///
/// # Safety
///
/// The caller holds the VM's file (so `vm` is live) and `so`'s, if any.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn amd_hwq_submit(
    sc: *mut sys::atrium_amd_softc,
    client: *mut sys::atrium_amd_client,
    ring_va: u64,
    n_dwords: u32,
    engine: u32,
    vm: *mut sys::atrium_amd_vm,
    deadline_ns: u32,
    so: *mut sys::atrium_amd_syncobj,
    signal_value: u64,
) -> c_int {
    let dev = unsafe { Dev::from_raw(sc) };
    let Some(q) = queues(dev).for_engine(engine) else {
        return bindings::EINVAL as c_int;
    };
    let vmid = unsafe { (*vm).vmid };
    let mut st = sx_xlock(&q.st);
    match st.owner {
        // A user-mode queue: only its owner, in the owning VM, may submit through the kernel
        // too (it can only lose its own queued work).
        Some(o) if o != owner(client, vm) => return refused(dev, EBUSY),
        Some(_) => {}
        None => {
            if let Err(e) = q.wait_idle(dev, &mut st, vmid) {
                return refused(dev, e);
            }
        }
    }
    // Room for the completion? Only the ISR changes the list while we hold the queue, and it
    // only shortens it.
    if !so.is_null() && !dev.q_room(q.qid) {
        return bindings::EBUSY as c_int;
    }
    // Stage the frame deadline, program, register the completion, ring: one sequence.
    dev.q_deadline(deadline_ns);
    let off = match dev.queue_program(ring_va, engine, vmid) {
        Ok(off) => off,
        Err(e) => return e.as_c_type(),
    };
    st.vmid = vmid;
    st.wptr = n_dwords;
    st.generation += 1;
    dev.q_started(q.qid, so, signal_value);
    dev.q_doorbell(off, n_dwords, !so.is_null());
    drop(st);
    dev.watchdog_arm();
    0
}

/// Backend queue_map: make `client` the exclusive owner of `engine`'s queue, running the ring
/// at `ring_va` in `vm`. Returns the doorbell's page offset in `*doorbell_off`.
///
/// # Safety
///
/// As `amd_hwq_submit`; `doorbell_off` is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn amd_hwq_map(
    sc: *mut sys::atrium_amd_softc,
    client: *mut sys::atrium_amd_client,
    vm: *mut sys::atrium_amd_vm,
    ring_va: u64,
    engine: u32,
    doorbell_off: *mut u32,
) -> c_int {
    let dev = unsafe { Dev::from_raw(sc) };
    let Some(q) = queues(dev).for_engine(engine) else {
        return bindings::EINVAL as c_int;
    };
    let vmid = unsafe { (*vm).vmid };
    let me = owner(client, vm);
    let mut st = sx_xlock(&q.st);
    match st.owner {
        Some(o) if o != me => return refused(dev, EBUSY),
        Some(_) => {}
        None => {
            if let Err(e) = q.wait_idle(dev, &mut st, vmid) {
                return refused(dev, e);
            }
        }
    }
    match dev.queue_program(ring_va, engine, vmid) {
        Ok(off) => {
            st.owner = Some(me);
            st.vmid = vmid;
            st.wptr = 0; // user space rings it from here on
            st.generation += 1;
            unsafe { *doorbell_off = off };
            0
        }
        Err(e) => e.as_c_type(),
    }
}

/// Backend mmap_single: the doorbell page of a queue `client` owns, as a revocable pager
/// object. EACCES for any other offset.
///
/// # Safety
///
/// The arguments are d_mmap_single's.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn amd_hwq_mmap_single(
    sc: *mut sys::atrium_amd_softc,
    client: *mut sys::atrium_amd_client,
    offset: *mut vm_ooffset_t,
    size: vm_size_t,
    object: *mut vm_object_t,
    nprot: c_int,
) -> c_int {
    let dev = unsafe { Dev::from_raw(sc) };
    let base = dev.doorbell_pa();
    if size != bindings::PAGE_SIZE as vm_size_t || base == 0 {
        return bindings::EINVAL as c_int;
    }
    let off = unsafe { *offset };
    for q in &queues(dev).0 {
        let mut st = sx_xlock(&q.st);
        if st.owner.map(|o| o.client) != Some(client as usize) || q.doorbell_off as vm_ooffset_t != off {
            continue;
        }
        if st.grant.is_none() {
            match doorbell::grant(dev, base + q.doorbell_off as u64, nprot) {
                Ok(g) => st.grant = Some(g),
                Err(e) => return e.as_c_type(),
            }
        }
        // A reference for the mapping; ours stays with the queue until it is revoked.
        let obj = st.grant.as_ref().map(|g| g.clone().into_raw()).unwrap_or(core::ptr::null_mut());
        unsafe {
            *object = obj;
            *offset = 0;
        }
        return 0;
    }
    // Not a doorbell of a queue this client owns.
    bindings::EACCES as c_int
}

/// The client closed its last fd on the device: give back its queues.
///
/// # Safety
///
/// `sc` is an attached softc.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn amd_hwq_client_close(sc: *mut sys::atrium_amd_softc, client: *mut sys::atrium_amd_client) {
    let dev = unsafe { Dev::from_raw(sc) };
    for q in &queues(dev).0 {
        let mut st = sx_xlock(&q.st);
        if st.owner.map(|o| o.client) == Some(client as usize) {
            q.dequeue(dev, &mut st);
        }
    }
    reap(dev);
}

/// A VM is going away: stop every queue programmed under its VMID (whatever is still queued
/// there belongs to a VM that no longer exists), then point the VMID at the all-invalid page
/// directory so nothing can walk the page tables the caller is about to free.
///
/// # Safety
///
/// `sc` is an attached softc.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn amd_hwq_vm_gone(sc: *mut sys::atrium_amd_softc, vmid: u16) {
    let dev = unsafe { Dev::from_raw(sc) };
    if vmid == 0 || dev.try_state().is_none() {
        return;
    }
    for q in &queues(dev).0 {
        let mut st = sx_xlock(&q.st);
        if st.vmid == vmid {
            q.dequeue(dev, &mut st);
        }
    }
    dev.vm_null_pd(vmid);
    reap(dev);
}

/// Device reset, before the FLR: hold every queue (no submit, map or doorbell mmap starts
/// until restore) and drop all queue state, since the FLR drops the rings themselves. Owners
/// lose their doorbell mappings and must QUEUE_MAP again.
///
/// # Safety
///
/// `sc` is an attached softc; `amd_hwq_reset_restore` follows on the same thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn amd_hwq_reset_prepare(sc: *mut sys::atrium_amd_softc) {
    let dev = unsafe { Dev::from_raw(sc) };
    for q in &queues(dev).0 {
        let mut st = sx_xlock(&q.st);
        q.dequeue(dev, &mut st);
        st.leak(); // held until amd_hwq_reset_restore
    }
}

/// Device reset, after the restore: let the queues be used again.
///
/// # Safety
///
/// `amd_hwq_reset_prepare` ran on this thread and nothing has released its holds.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn amd_hwq_reset_restore(sc: *mut sys::atrium_amd_softc) {
    let dev = unsafe { Dev::from_raw(sc) };
    for q in queues(dev).0.iter().rev() {
        unsafe { q.st.xunlock_leaked() };
    }
    reap(dev);
}

/// The hang watchdog pass (queue.c's watchdog task). Returns 1 while some kernel-rung queue
/// is still busy, so the task runs again.
///
/// # Safety
///
/// `sc` is an attached softc.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn amd_rs_watchdog(sc: *mut sys::atrium_amd_softc) -> c_int {
    let dev = unsafe { Dev::from_raw(sc) };
    let mut active = false;
    for q in &queues(dev).0 {
        // A submitter holding the queue runs the same check while it waits; look again
        // next round.
        let Some(mut st) = sx_try_xlock(&q.st) else {
            active = true;
            continue;
        };
        if !q.check_hang(dev, &mut st) && st.owner.is_none() && q.busy(dev, &st) {
            active = true;
        }
    }
    active as c_int
}

/// Detach: stop every queue (no client is left, so none has an owner or a grant).
///
/// # Safety
///
/// `sc` is an attached softc.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn amd_rs_queues_stop(sc: *mut sys::atrium_amd_softc) {
    let dev = unsafe { Dev::from_raw(sc) };
    if dev.try_state().is_none() {
        return;
    }
    for q in &queues(dev).0 {
        let mut st = sx_xlock(&q.st);
        q.dequeue(dev, &mut st);
    }
}

