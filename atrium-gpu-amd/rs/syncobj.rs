/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * syncobj.rs — timeline syncobj: a monotonic 64-bit counter exposed as an fd
 * (ABI-v2 §5.6). Was sync.c.
 *
 * A submission signals the syncobj on completion; user space waits either
 * blocking (SYNCOBJ_WAIT) or through kqueue: the fd is EVFILT_READ-able with the
 * wait threshold passed in the kevent `data` field, so frescod folds "GPU frame
 * done" into the same kevent() as its sockets and input.
 *
 * C code knows a syncobj only as an opaque `struct atrium_amd_syncobj *`: the
 * EOP handler (irq.c) signals or fails it under sc->lock, and the display
 * module installs a watcher and asks whether a fence value is reached. Those
 * pointers stay valid because the pending list is scrubbed (amd_pending_scrub)
 * before a syncobj is freed, and the display holds a file reference.
 *
 * Lock order: sc->lock, then the syncobj's own lock.
 */

use crate::backend::{Dev, WatchFn};
use crate::sys;
use core::ffi::{c_int, c_long, c_void};
use core::pin::Pin;
use kpi::bindings::{self, file, knote, thread};
use kpi::boxed::Box;
use kpi::event::{FilterOps, Knote, SelInfo};
use kpi::file::{self as kfile, FileRef};
use kpi::kobj::AsCType;
use kpi::prelude::*;
use kpi::sync::mtx::{Mutex, MutexGuard, SBT_1MS, mtx_init, mtx_lock, mtx_sleep_sbt, sbinuptime};

struct State {
    value: u64,
    /// Sticky: a completion owed here was lost (EIO). Waiters for a value not yet reached
    /// get it instead of sleeping to their timeout.
    error: c_int,
    /// One in-kernel watcher (the display's in-fence): called under the lock after every
    /// signal or failure. It must not sleep or take sc->lock; the display only enqueues a task.
    watch: Option<WatchFn>,
    watch_arg: *mut c_void,
}

pub struct SyncObj {
    // Field order is drop order: the knote list goes first, while its lock still exists;
    // the count last, after everything that could touch the softc.
    sel: SelInfo,
    state: Mutex<State>,
    dev: Dev,
    _count: LiveCount,
}

// SAFETY: everything mutable is behind `state`; the C pointers in it (watch_arg) are the
// display's, used only under the lock.
unsafe impl Send for SyncObj {}
unsafe impl Sync for SyncObj {}

kpi::file_ops!(SyncObj, |ops| {
    ops.fo_kqfilter = Some(syncobj_kqfilter);
});

/// One live syncobj in the softc's count; module detach waits for it to reach 0.
struct LiveCount(Dev);

impl LiveCount {
    fn new(dev: Dev) -> Self {
        dev.syncobj_count(1);
        LiveCount(dev)
    }
}

impl Drop for LiveCount {
    fn drop(&mut self) {
        // The last touch of the softc.
        self.0.syncobj_count(-1);
    }
}

impl Drop for SyncObj {
    fn drop(&mut self) {
        // Drop any completion the ISR still owes us before we are freed. The scrub takes
        // sc->lock, which the ISR holds while it signals, so once it returns the ISR can no
        // longer reach us.
        self.dev.pending_scrub(self.as_c());
    }
}

impl SyncObj {
    /// The pointer C code knows this syncobj by.
    pub fn as_c(&self) -> *mut sys::atrium_amd_syncobj {
        core::ptr::from_ref(self).cast_mut().cast()
    }

    /// # Safety
    ///
    /// `so` came from `as_c` on a syncobj that is still alive.
    pub(crate) unsafe fn from_c<'a>(so: *mut sys::atrium_amd_syncobj) -> &'a SyncObj {
        unsafe { &*so.cast::<SyncObj>() }
    }

    /// The sleep channel blocking waiters use.
    fn chan(&self) -> *const c_void {
        core::ptr::from_ref(&self.state).cast()
    }

    /// Wake blocking waiters, kqueue knotes and the watcher. Lock held.
    fn notify(&self, g: &MutexGuard<'_, State>) {
        unsafe { bindings::wakeup(self.chan()) };
        self.sel.knote_locked(g, 0);
        if let Some(w) = g.watch {
            unsafe { w(g.watch_arg) };
        }
    }

    /// Advance the timeline to `value` (never backwards).
    pub fn signal(&self, value: u64) {
        let mut g = mtx_lock(&self.state);
        if value > g.value {
            g.value = value;
        }
        self.notify(&g);
    }

    /// A completion owed here will never come (queue stopped: VM gone, hang, reset).
    pub fn fail(&self, error: c_int) {
        let mut g = mtx_lock(&self.state);
        if g.error == 0 {
            g.error = error;
        }
        self.notify(&g);
    }

    pub fn value(&self) -> u64 {
        mtx_lock(&self.state).value
    }

    /// Wait until the timeline reaches `value`, for at most `timeout_ms` (0: check once).
    /// Returns an errno: 0, the sticky error, EINTR/ERESTART on a signal, or EWOULDBLOCK.
    pub fn wait(&self, value: u64, timeout_ms: u32) -> c_int {
        // An absolute deadline: no int-ticks overflow on long timeouts (review §A2.6).
        let end = sbinuptime().saturating_add(timeout_ms as i64 * SBT_1MS);
        let mut g = mtx_lock(&self.state);
        let mut serr = 0;
        while g.value < value && g.error == 0 && timeout_ms != 0 {
            let (g2, r) = mtx_sleep_sbt(
                self.chan(),
                g,
                bindings::PCATCH as c_int,
                c"amdsyncw",
                end,
                0,
                bindings::C_ABSOLUTE as c_int,
            );
            g = g2;
            serr = r;
            if serr != 0 {
                break;
            }
        }
        if g.value >= value {
            0
        } else if g.error != 0 {
            g.error
        } else if serr == bindings::EINTR as c_int || serr == bindings::ERESTART {
            serr
        } else {
            bindings::EWOULDBLOCK as c_int
        }
    }

    /// 1 if the timeline reached `value`, -errno if the work that would reach it was lost,
    /// 0 otherwise.
    pub fn ready(&self, value: u64) -> c_int {
        let g = mtx_lock(&self.state);
        if g.value >= value {
            1
        } else if g.error != 0 {
            -g.error
        } else {
            0
        }
    }

    pub fn set_watch(&self, f: Option<WatchFn>, arg: *mut c_void) {
        let mut g = mtx_lock(&self.state);
        g.watch = f;
        g.watch_arg = arg;
    }
}

/// SYNCOBJ_CREATE: a new syncobj fd at value 0.
pub fn create_fd(dev: Dev) -> Result<c_int> {
    let so = Box::new(
        SyncObj {
            sel: SelInfo::new(),
            state: Mutex::new(State {
                value: 0,
                error: 0,
                watch: None,
                watch_arg: core::ptr::null_mut(),
            }),
            dev,
            _count: LiveCount::new(dev),
        },
        M_WAITOK,
    );
    // SAFETY: the object is boxed and never moves again; the knote list is initialised with
    // the lock it lives beside and is dropped before it.
    unsafe {
        mtx_init(Pin::new_unchecked(&so.state), c"amdsync", None, None);
        so.sel.init_mtx(&so.state);
    }
    kfile::install(so, bindings::FREAD as c_int)
}

// --- kqueue: fire when value >= the threshold the knote carries ---

static SYNCOBJ_FILTOPS: FilterOps = FilterOps::fd(syncobj_filt_detach, syncobj_filt_event);

unsafe extern "C" fn syncobj_kqfilter(fp: *mut file, kn: *mut knote) -> c_int {
    // SAFETY: the kernel calls this with a held, live syncobj file.
    let so = unsafe { &*(*fp).f_data.cast::<SyncObj>() };
    let kn = unsafe { Knote::from_raw(kn) };
    if kn.filter() != bindings::EVFILT_READ as i16 {
        return bindings::EINVAL as c_int;
    }
    let _g = mtx_lock(&so.state);
    // SAFETY: the knote is detached (f_detach) before the file, and so the syncobj, goes away.
    unsafe {
        kn.set_ops(&SYNCOBJ_FILTOPS, core::ptr::from_ref(so).cast());
        so.sel.add(kn.as_ptr(), true);
    }
    0
}

unsafe extern "C" fn syncobj_filt_detach(kn: *mut knote) {
    let kn = unsafe { Knote::from_raw(kn) };
    // SAFETY: kn_hook was set to the live syncobj in syncobj_kqfilter.
    let so = unsafe { &*kn.hook::<SyncObj>() };
    let _g = mtx_lock(&so.state);
    unsafe { so.sel.remove(kn.as_ptr(), true) };
}

unsafe extern "C" fn syncobj_filt_event(kn: *mut knote, _hint: c_long) -> c_int {
    let kn = unsafe { Knote::from_raw(kn) };
    // SAFETY: the knlist calls f_event with the syncobj's lock held (knote_locked or the
    // kqueue's own scan, which takes the list lock), so the state can be read in place.
    let so = unsafe { &*kn.hook::<SyncObj>() };
    let st = unsafe { &*so.state.data_ptr() };
    kn.set_data(st.value as i64);
    // kn_sdata is the threshold from the kevent `data` field.
    if st.value >= kn.sdata() as u64 {
        return 1;
    }
    if st.error != 0 {
        // The work that would have reached the threshold is gone.
        kn.set_eof(st.error as u32);
        return 1;
    }
    0
}

// --- exports for C (irq.c and, through the backend vtable, the display module) ---

#[unsafe(no_mangle)]
pub unsafe extern "C" fn amd_syncobj_fget(
    _td: *mut thread,
    fd: c_int,
    out_fp: *mut *mut file,
    out_so: *mut *mut sys::atrium_amd_syncobj,
) -> c_int {
    match FileRef::<SyncObj>::get(fd) {
        Ok(r) => {
            unsafe {
                *out_so = r.as_c();
                *out_fp = r.into_raw(); // the caller fdrop()s it
            }
            0
        }
        Err(e) => e.as_c_type(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn amd_syncobj_signal(so: *mut sys::atrium_amd_syncobj, value: u64) {
    unsafe { SyncObj::from_c(so) }.signal(value);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn amd_syncobj_fail(so: *mut sys::atrium_amd_syncobj, error: c_int) {
    unsafe { SyncObj::from_c(so) }.fail(error);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn amd_syncobj_watch(
    so: *mut sys::atrium_amd_syncobj,
    f: Option<WatchFn>,
    arg: *mut c_void,
) {
    unsafe { SyncObj::from_c(so) }.set_watch(f, arg);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn amd_syncobj_ready(so: *mut sys::atrium_amd_syncobj, value: u64) -> c_int {
    unsafe { SyncObj::from_c(so) }.ready(value)
}
