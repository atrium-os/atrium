/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * pending.rs — the completions each hardware queue owes, and the hang clock.
 * Was the pending lists in irq.c and the queue helpers in queue.c.
 *
 * A submit that signals a syncobj registers the completion before it rings
 * the doorbell; the end-of-pipe interrupt retires the head of that queue's
 * list (review §A2.2: an EOP names its queue, so it can only retire that
 * queue's work, never another VM's). A queue that is stopped, hangs or is
 * reset fails what it owed with EIO.
 *
 * The lists are guarded by sc->lock, the softc mutex the base module's ISR
 * holds while it calls the cause handlers. It is a C lock, so Rust cannot own
 * it; instead every access goes through `ScLocked`, a token that exists only
 * while sc->lock is held: `Dev::sc_lock` takes the lock (and drops it with the
 * token), and the ISR entry points assert it. The lists are reachable only
 * through `ScLocked::q`, which takes the token mutably, so two borrows of the
 * lists can't overlap.
 *
 * The lists hold raw syncobj pointers. They stay valid because a syncobj's
 * close scrubs its entries under sc->lock (`amd_pending_scrub`) before it is
 * freed, and the ISR signals under the same lock: no reference counting, and
 * no free from the ISR. A scrubbed entry stays in place (as None) so it still
 * consumes the EOP its submit will raise.
 */

use crate::backend::Dev;
use crate::syncobj::SyncObj;
use crate::sys;
use core::cell::UnsafeCell;
use core::ffi::c_int;
use core::ptr::NonNull;
use kpi::kobj::AsCType;
use kpi::prelude::*;

const NHWQ: usize = sys::ATRIUM_AMD_NHWQ as usize;
/// Signalled submits in flight per queue.
const MAX_PENDING: usize = sys::ATRIUM_AMD_MAX_PENDING as usize;

#[derive(Copy, Clone)]
struct Owed {
    /// None: the submit signals nothing, or its syncobj has closed since.
    so: Option<NonNull<sys::atrium_amd_syncobj>>,
    value: u64,
}

const NONE: Owed = Owed { so: None, value: 0 };

/// One queue's owed completions (a ring, oldest at `head`) and its hang clock.
pub struct QPend {
    e: [Owed; MAX_PENDING],
    head: usize,
    n: usize,
    /// The tick of the queue's last sign of life: a submit, an EOP, or its read pointer moving.
    progress: c_int,
    last_rptr: u32,
    /// Its current work hit a VM fault.
    faulted: bool,
}

impl QPend {
    const fn new() -> Self {
        QPend { e: [NONE; MAX_PENDING], head: 0, n: 0, progress: 0, last_rptr: 0, faulted: false }
    }

    pub fn owes(&self) -> bool {
        self.n > 0
    }

    pub fn has_room(&self) -> bool {
        self.n < MAX_PENDING
    }

    fn push(&mut self, o: Owed) -> bool {
        if self.n >= MAX_PENDING {
            return false;
        }
        self.e[(self.head + self.n) % MAX_PENDING] = o;
        self.n += 1;
        true
    }

    fn pop(&mut self) -> Option<Owed> {
        if self.n == 0 {
            return None;
        }
        let o = self.e[self.head];
        self.head = (self.head + 1) % MAX_PENDING;
        self.n -= 1;
        Some(o)
    }

    /// Fail every owed completion with `error` (the work behind them is gone). Returns how
    /// many syncobjs were failed.
    fn fail_all(&mut self, error: c_int) -> u32 {
        let mut n = 0;
        while let Some(o) = self.pop() {
            if let Some(so) = o.so {
                // SAFETY: live: a syncobj scrubs itself out under sc->lock before it is freed.
                unsafe { SyncObj::from_c(so.as_ptr()) }.fail(error);
                n += 1;
            }
        }
        self.head = 0;
        n
    }

    fn scrub(&mut self, so: *mut sys::atrium_amd_syncobj) {
        for i in 0..self.n {
            let e = &mut self.e[(self.head + i) % MAX_PENDING];
            if e.so.is_some_and(|p| p.as_ptr() == so) {
                e.so = None;
            }
        }
    }
}

/// Every queue's list. Only reachable through `ScLocked`.
pub struct Pending(UnsafeCell<[QPend; NHWQ]>);

// SAFETY: only accessed with sc->lock held (ScLocked).
unsafe impl Send for Pending {}
unsafe impl Sync for Pending {}

impl Pending {
    pub const fn new() -> Self {
        Pending(UnsafeCell::new([const { QPend::new() }; NHWQ]))
    }
}

/// Proof that sc->lock is held.
pub struct ScLocked {
    dev: Dev,
    /// Unlock on drop: taken by `Dev::sc_lock`, not by the ISR's caller.
    owned: bool,
}

impl ScLocked {
    /// Take sc->lock.
    pub fn lock(dev: Dev) -> Self {
        dev.sc_mtx_lock();
        ScLocked { dev, owned: true }
    }

    /// # Safety
    ///
    /// The caller holds sc->lock for as long as the token lives (the ISR's cause handlers).
    unsafe fn assume_held(dev: Dev) -> Self {
        dev.sc_mtx_assert();
        ScLocked { dev, owned: false }
    }

    /// Queue `qid`'s list. Bounds-checked: `qid` comes from a queue or an IH cookie.
    pub fn q(&mut self, qid: u32) -> &mut QPend {
        // SAFETY: sc->lock is held, and `&mut self` keeps this the only borrow.
        let all = unsafe { &mut *self.dev.state().pending.0.get() };
        &mut all[qid as usize]
    }
}

impl Drop for ScLocked {
    fn drop(&mut self) {
        if self.owned {
            self.dev.sc_mtx_unlock();
        }
    }
}

// --- queue.rs's side ---

/// New work was programmed on `qid`; it signals `so` with `value` on completion (if `so` is
/// set). The caller rings the doorbell after this, so a synchronous drain still finds the
/// entry. The caller checked `has_room` under the queue's lock, which is the only pusher.
pub fn started(dev: Dev, qid: u32, so: *mut sys::atrium_amd_syncobj, value: u64) {
    let ticks = dev.ticks();
    let mut g = ScLocked::lock(dev);
    let q = g.q(qid);
    // New work, possibly another VM's: a fault reported before this belonged to the work
    // before it, unless that work still owes completions (same VM), which must still fail.
    if !q.owes() {
        q.faulted = false;
    }
    if let Some(so) = NonNull::new(so) {
        let pushed = q.push(Owed { so: Some(so), value });
        debug_assert!(pushed);
    }
    q.progress = ticks;
    q.last_rptr = 0;
}

/// `qid` was stopped: its work will raise no EOP, so fail what it owed.
pub fn stopped(dev: Dev, qid: u32, was_busy: bool) {
    let mut g = ScLocked::lock(dev);
    let n = g.q(qid).fail_all(EIO.as_c_type());
    g.q(qid).faulted = false;
    dev.count_locked(sys::AMD_RS_CNT_FENCES_FAILED, n);
    if was_busy {
        dev.count_locked(sys::AMD_RS_CNT_DEQUEUES, 1);
    }
}

pub fn owes(dev: Dev, qid: u32) -> bool {
    ScLocked::lock(dev).q(qid).owes()
}

pub fn has_room(dev: Dev, qid: u32) -> bool {
    ScLocked::lock(dev).q(qid).has_room()
}

/// The hang check (review §A2.6) for a busy kernel-rung queue: true if it must be stopped
/// now, for a VM fault, or for no progress (read-pointer movement or an EOP) for hang_ms.
pub fn hung(dev: Dev, qid: u32, vmid: u16, rptr: u32) -> bool {
    let now = dev.ticks();
    let what = {
        let mut g = ScLocked::lock(dev);
        let limit = dev.hang_ticks_locked();
        let q = g.q(qid);
        if q.faulted {
            Some(sys::AMD_RS_STOP_FAULT)
        } else if rptr != q.last_rptr {
            q.last_rptr = rptr;
            q.progress = now;
            None
        } else if now.wrapping_sub(q.progress) >= limit {
            dev.count_locked(sys::AMD_RS_CNT_HANGS, 1);
            Some(sys::AMD_RS_STOP_HANG)
        } else {
            None
        }
    };
    // Reported outside sc->lock.
    match what {
        Some(w) => {
            dev.q_report_stop(qid, vmid, w);
            true
        }
        None => false,
    }
}

// --- exports for C ---

/// The EOP cause handler's body (irq.c), sc->lock held by the base ISR. `eop[q]` is how many
/// end-of-pipe interrupts came from queue q this pass, `fault[q]` how many VM faults; each
/// EOP retires the head of its own queue's list. A ring that parked on a cross-queue WAIT is
/// retired on the later pass that unblocked it.
///
/// # Safety
///
/// `sc` is attached with rs_state set, sc->lock is held, and `eop`/`fault` point to NHWQ
/// counters each.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn amd_rs_eop(sc: *mut sys::atrium_amd_softc, eop: *const u32, fault: *const u32) {
    let dev = unsafe { Dev::from_raw(sc) };
    let (eop, fault) = unsafe {
        (core::slice::from_raw_parts(eop, NHWQ), core::slice::from_raw_parts(fault, NHWQ))
    };
    let ticks = dev.ticks();
    let mut g = unsafe { ScLocked::assume_held(dev) };
    let mut failed = 0;
    for qid in 0..NHWQ {
        let mut n = eop[qid];
        if n == 0 {
            continue;
        }
        let q = g.q(qid as u32);
        q.progress = ticks;
        if fault[qid] != 0 {
            q.faulted = true;
        }
        while n > 0 {
            let Some(o) = q.pop() else { break };
            if let Some(so) = o.so {
                // SAFETY: live, as in fail_all.
                let so = unsafe { SyncObj::from_c(so.as_ptr()) };
                // Work that faulted ran with stores dropped and loads reading zero: it
                // finished, but not correctly.
                if q.faulted {
                    so.fail(EIO.as_c_type());
                    failed += 1;
                } else {
                    so.signal(o.value);
                }
            }
            n -= 1;
        }
    }
    if failed > 0 {
        dev.count_locked(sys::AMD_RS_CNT_FENCES_FAILED, failed);
    }
}

/// The VM fault cause handler (irq.c), sc->lock held: queue `qid`'s current work faulted, so
/// its completions fail with EIO and the watchdog stops it.
///
/// # Safety
///
/// As `amd_rs_eop`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn amd_rs_q_faulted_locked(sc: *mut sys::atrium_amd_softc, qid: u32) {
    let dev = unsafe { Dev::from_raw(sc) };
    if (qid as usize) < NHWQ {
        unsafe { ScLocked::assume_held(dev) }.q(qid).faulted = true;
    }
}

/// Remove every pending entry for a syncobj: its close calls this before freeing it, so the
/// ISR never touches a freed syncobj.
pub fn scrub(dev: Dev, so: *mut sys::atrium_amd_syncobj) {
    let mut g = ScLocked::lock(dev);
    for qid in 0..NHWQ as u32 {
        g.q(qid).scrub(so);
    }
}
