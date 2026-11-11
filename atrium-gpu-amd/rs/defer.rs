/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * defer.rs — deferred BO frees (isolation break 2). Was the deferred list in
 * queue.c.
 *
 * When a BO's last fd closes, its PTEs are cleared at once, but work already
 * queued under one of its VMs may still be reading or writing its pages. Such
 * a BO waits here until every queue that was busy at close time has drained,
 * or has been reprogrammed or stopped since (its generation changed, which
 * only happens once it drained or was dequeued). Then its backing is freed.
 *
 * The BO stays a `Box<Bo>` the whole way: owned by its file, then by this
 * list, then dropped. C never holds it. queue.c keeps the timer (defer_task)
 * that runs `amd_rs_reap`, and the per-queue busy/generation state that
 * `Dev::queues_now` reads.
 */

use crate::backend::Dev;
use crate::bo::Bo;
use crate::queue::Queues;
use crate::quota::Quotas;
use crate::pending::Pending;
use crate::sys;
use core::ffi::{c_int, c_void};
use core::pin::Pin;
use kpi::boxed::Box;
use kpi::prelude::*;
use kpi::sync::mtx::{Mutex, mtx_init, mtx_lock};

const NHWQ: usize = sys::ATRIUM_AMD_NHWQ as usize;

/// Which queues were busy when the BO closed, and their generations then.
#[derive(Copy, Clone)]
pub struct BusyAt {
    pub mask: u32,
    pub gens: [u64; NHWQ],
}

impl BusyAt {
    /// Has every queue that was busy then moved on, given the queues' state `now`?
    fn retired(&self, now: &BusyAt) -> bool {
        (0..NHWQ).all(|i| {
            let was_busy = self.mask & (1 << i) != 0;
            let same_work = now.gens[i] == self.gens[i];
            let still_busy = now.mask & (1 << i) != 0;
            !(was_busy && same_work && still_busy)
        })
    }
}

struct Deferred {
    bo: Box<Bo>,
    at: BusyAt,
    next: Option<Box<Deferred>>,
}

/// The front-end's per-device state (sc->rs_state).
pub struct DevState {
    deferred: Mutex<Option<Box<Deferred>>>,
    /// The hardware queues' state (queue.rs).
    pub queues: Queues,
    /// Per-uid BO charges (quota.rs).
    pub quotas: Quotas,
    /// The completions each queue owes (pending.rs; guarded by sc->lock).
    pub pending: Pending,
}

// SAFETY: everything is behind the mutex; a Bo is Send.
unsafe impl Send for DevState {}
unsafe impl Sync for DevState {}

impl DevState {
    /// # Safety
    ///
    /// `p` came from `amd_rs_dev_init` and `amd_rs_dev_fini` has not run.
    pub unsafe fn from_raw<'a>(p: *mut c_void) -> &'a DevState {
        unsafe { &*p.cast::<DevState>() }
    }
}

/// Hold `bo` until the queues in `at` have moved on (BoFile's drop).
pub fn defer(dev: Dev, bo: Box<Bo>, at: BusyAt) {
    let node = Box::new(Deferred { bo, at, next: None }, M_WAITOK);
    {
        let mut head = mtx_lock(&dev.state().deferred);
        let mut node = node;
        node.next = head.take();
        *head = Some(node);
    }
    dev.defer_count(1, 0);
    dev.defer_kick();
}

/// Free every deferred BO whose queues have moved on. Returns true if some still wait.
pub fn reap(dev: Dev) -> bool {
    let now = dev.queues_now();
    let mut done: Option<Box<Deferred>> = None;
    let left = {
        let mut head = mtx_lock(&dev.state().deferred);
        let mut keep: Option<Box<Deferred>> = None;
        let mut cur = head.take();
        while let Some(mut d) = cur {
            cur = d.next.take();
            let list = if d.at.retired(&now) { &mut done } else { &mut keep };
            d.next = list.take();
            *list = Some(d);
        }
        *head = keep;
        head.is_some()
    };
    // Outside the lock: bo_free may sleep.
    let mut n = 0;
    let mut cur = done;
    while let Some(mut d) = cur {
        cur = d.next.take();
        crate::bo::free_backing(dev, &mut d.bo); // backing store, bo_count--, uncharge
        n += 1;
        // `d` (and its Box<Bo>) drops here.
    }
    if n > 0 {
        dev.defer_count(-n, n);
    }
    left
}

// --- exports for C (queue.c) ---

/// Attach: the per-device state. NULL on failure.
#[unsafe(no_mangle)]
pub extern "C" fn amd_rs_dev_init() -> *mut c_void {
    let s: Box<DevState> = Box::new(
        DevState { deferred: Mutex::new(None), queues: Queues::new(), quotas: Quotas::new(), pending: Pending::new() },
        M_WAITOK,
    );
    // SAFETY: boxed, never moves again; the locks are destroyed by their drops.
    unsafe {
        mtx_init(Pin::new_unchecked(&s.deferred), c"amd_defer", None, None);
        s.queues.init();
        s.quotas.init();
    }
    Box::into_raw(s).cast()
}

/// Detach, after the last reap: nothing may be deferred any more.
///
/// # Safety
///
/// `p` came from `amd_rs_dev_init` and is not used again.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn amd_rs_dev_fini(p: *mut c_void) {
    if p.is_null() {
        return;
    }
    let s = unsafe { Box::<DevState>::from_raw(p.cast()) };
    // module.c refuses to detach while deferred_now > 0; a leftover would leak its pages,
    // never free them under the GPU.
    debug_assert!(mtx_lock(&s.deferred).is_none());
}

/// The deferred-free pass (queue.c's defer_task and watchdog). Returns 1 if BOs still wait.
///
/// # Safety
///
/// `sc` is an attached softc whose rs_state is set.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn amd_rs_reap(sc: *mut sys::atrium_amd_softc) -> c_int {
    reap(unsafe { Dev::from_raw(sc) }) as c_int
}
