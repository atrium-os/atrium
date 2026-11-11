//! pthreads and semaphores on futexes (`_umtx_op` on FreeBSD), in glibc's object sizes.
//!
//! Only the first 32-bit word of each glibc object is ours: a mutex's word is glibc's own
//! `__lock` (so the all-zero `PTHREAD_MUTEX_INITIALIZER` is unlocked), a condition
//! variable's is a sequence counter, a semaphore's is its value. `abi.rs` and its layout
//! test pin the sizes.
use crate::abi;
use core::ffi::c_void;
use core::sync::atomic::{AtomicU32, Ordering};
use hospes_rt::{sys, thread, tls};

unsafe fn word<'a, T>(p: *mut T) -> &'a AtomicU32 {
    &*(p as *const AtomicU32)
}

/// Deadline in glibc's `struct timespec` (CLOCK_REALTIME unless said otherwise).
#[repr(C)]
pub struct Timespec {
    sec: i64,
    nsec: i64,
}

fn past(t: &Timespec) -> bool {
    let (s, ns) = sys::realtime();
    (s, ns) >= (t.sec, t.nsec)
}

// ---- threads -------------------------------------------------------------------------

pub type Start = extern "C" fn(*mut c_void) -> *mut c_void;

pub unsafe extern "C" fn pthread_create(t: *mut usize, _attr: *const c_void, f: Start, arg: *mut c_void) -> i32 {
    let entry: thread::Entry = core::mem::transmute(f);
    match thread::spawn(entry, arg as usize, thread::DEFAULT_STACK) {
        Ok(th) => {
            *t = th.into_raw() as usize;
            0
        }
        Err(_) => abi::EAGAIN,
    }
}

pub unsafe extern "C" fn pthread_join(t: usize, ret: *mut *mut c_void) -> i32 {
    if t == 0 || t == tls::current() as usize {
        return abi::EINVAL;
    }
    let r = thread::Thread::from_raw(t as *mut tls::ThreadState).join();
    if !ret.is_null() {
        *ret = r as *mut c_void;
    }
    0
}

pub extern "C" fn pthread_self() -> usize {
    tls::current() as usize
}

pub unsafe extern "C" fn pthread_attr_init(a: *mut u8) -> i32 {
    core::ptr::write_bytes(a, 0, abi::SIZEOF_PTHREAD_ATTR_T);
    0
}
pub extern "C" fn pthread_attr_destroy(_a: *mut u8) -> i32 {
    0
}
/// Scheduling requests are accepted and ignored (spec §6.2).
pub extern "C" fn pthread_attr_setscope(_a: *mut u8, _s: i32) -> i32 {
    0
}
pub extern "C" fn pthread_attr_setschedpolicy(_a: *mut u8, _p: i32) -> i32 {
    0
}

// ---- mutex: 0 free, 1 held, 2 held with waiters ------------------------------------------

pub unsafe extern "C" fn pthread_mutex_init(m: *mut u8, _attr: *const c_void) -> i32 {
    core::ptr::write_bytes(m, 0, abi::SIZEOF_PTHREAD_MUTEX_T);
    0
}
pub unsafe extern "C" fn pthread_mutex_destroy(_m: *mut u8) -> i32 {
    0
}
pub unsafe extern "C" fn pthread_mutex_lock(m: *mut u8) -> i32 {
    let w = word(m);
    if w.compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed).is_err() {
        lock_slow(w);
    }
    0
}
#[cold]
fn lock_slow(w: &AtomicU32) {
    for _ in 0..100 {
        if w.compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed).is_ok() {
            return;
        }
        core::hint::spin_loop();
    }
    while w.swap(2, Ordering::Acquire) != 0 {
        unsafe { sys::futex_wait(w.as_ptr(), 2) };
    }
}
pub unsafe extern "C" fn pthread_mutex_unlock(m: *mut u8) -> i32 {
    let w = word(m);
    if w.swap(0, Ordering::Release) == 2 {
        sys::futex_wake(w.as_ptr(), 1);
    }
    0
}

// ---- condition variables: a sequence counter ---------------------------------------------

pub unsafe extern "C" fn pthread_cond_init(c: *mut u8, _attr: *const c_void) -> i32 {
    core::ptr::write_bytes(c, 0, abi::SIZEOF_PTHREAD_COND_T);
    0
}
pub unsafe extern "C" fn pthread_cond_destroy(_c: *mut u8) -> i32 {
    0
}
unsafe fn cond_wait(c: *mut u8, m: *mut u8, deadline: Option<&Timespec>) -> i32 {
    let seq = word(c);
    let v = seq.load(Ordering::Acquire);
    pthread_mutex_unlock(m);
    let timed_out = match deadline {
        None => {
            sys::futex_wait(seq.as_ptr(), v);
            false
        }
        Some(t) => sys::futex_wait_until_realtime(seq.as_ptr(), v, t.sec, t.nsec) || past(t),
    };
    // Relock as contended: other waiters may be queued on the mutex.
    let w = word(m);
    while w.swap(2, Ordering::Acquire) != 0 {
        sys::futex_wait(w.as_ptr(), 2);
    }
    if timed_out && seq.load(Ordering::Acquire) == v {
        abi::ETIMEDOUT
    } else {
        0
    }
}
pub unsafe extern "C" fn pthread_cond_wait(c: *mut u8, m: *mut u8) -> i32 {
    cond_wait(c, m, None)
}
pub unsafe extern "C" fn pthread_cond_timedwait(c: *mut u8, m: *mut u8, t: *const Timespec) -> i32 {
    if t.is_null() || (*t).nsec < 0 || (*t).nsec >= 1_000_000_000 {
        return abi::EINVAL;
    }
    cond_wait(c, m, Some(&*t))
}
pub unsafe extern "C" fn pthread_cond_broadcast(c: *mut u8) -> i32 {
    let seq = word(c);
    seq.fetch_add(1, Ordering::Release);
    sys::futex_wake(seq.as_ptr(), i32::MAX as u32);
    0
}
pub unsafe extern "C" fn pthread_cond_signal(c: *mut u8) -> i32 {
    let seq = word(c);
    seq.fetch_add(1, Ordering::Release);
    sys::futex_wake(seq.as_ptr(), 1);
    0
}

// ---- semaphores ---------------------------------------------------------------------------

pub unsafe extern "C" fn sem_init(s: *mut u8, pshared: i32, value: u32) -> i32 {
    if pshared != 0 {
        // No process-shared objects: there is no other process.
        crate::set_errno(abi::ENOSYS);
        return -1;
    }
    core::ptr::write_bytes(s, 0, abi::SIZEOF_SEM_T);
    word(s).store(value, Ordering::Release);
    0
}
pub unsafe extern "C" fn sem_destroy(_s: *mut u8) -> i32 {
    0
}
pub unsafe extern "C" fn sem_post(s: *mut u8) -> i32 {
    let w = word(s);
    w.fetch_add(1, Ordering::Release);
    sys::futex_wake(w.as_ptr(), 1);
    0
}
unsafe fn sem_take(w: &AtomicU32) -> bool {
    let mut v = w.load(Ordering::Acquire);
    while v > 0 {
        match w.compare_exchange_weak(v, v - 1, Ordering::Acquire, Ordering::Acquire) {
            Ok(_) => return true,
            Err(x) => v = x,
        }
    }
    false
}
pub unsafe extern "C" fn sem_wait(s: *mut u8) -> i32 {
    let w = word(s);
    while !sem_take(w) {
        sys::futex_wait(w.as_ptr(), 0);
    }
    0
}
pub unsafe extern "C" fn sem_trywait(s: *mut u8) -> i32 {
    if sem_take(word(s)) {
        0
    } else {
        crate::set_errno(abi::EAGAIN);
        -1
    }
}
pub unsafe extern "C" fn sem_timedwait(s: *mut u8, t: *const Timespec) -> i32 {
    let w = word(s);
    loop {
        if sem_take(w) {
            return 0;
        }
        if t.is_null() || (*t).nsec < 0 || (*t).nsec >= 1_000_000_000 {
            crate::set_errno(abi::EINVAL);
            return -1;
        }
        if past(&*t) || sys::futex_wait_until_realtime(w.as_ptr(), 0, (*t).sec, (*t).nsec) {
            if sem_take(w) {
                return 0;
            }
            crate::set_errno(abi::ETIMEDOUT);
            return -1;
        }
    }
}
