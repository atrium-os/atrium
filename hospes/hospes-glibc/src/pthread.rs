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

/// Default thread stack, also what `getrlimit(RLIMIT_STACK)` reports.
pub const STACK: usize = thread::DEFAULT_STACK;

/// Our layout inside the caller's opaque `pthread_attr_t`: detach state, then stack size
/// (0: the default). Only our own functions read or write it.
#[repr(C)]
struct Attr {
    detached: u32,
    _pad: u32,
    stack: usize,
}
const _: () = assert!(core::mem::size_of::<Attr>() <= abi::SIZEOF_PTHREAD_ATTR_T);

/// What the start wrapper needs: the library's function and argument.
struct Start2 {
    f: Start,
    arg: *mut c_void,
}

/// Runs the library's start function, then its key destructors (POSIX: up to four rounds
/// while values remain).
extern "C" fn start_wrapper(p: usize) -> usize {
    let b = unsafe { alloc::boxed::Box::from_raw(p as *mut Start2) };
    let r = (b.f)(b.arg);
    drop(b);
    unsafe { run_key_destructors() };
    r as usize
}

/// Detached threads finished but not yet freed: swept on each `pthread_create`.
static DETACHED: [core::sync::atomic::AtomicUsize; 64] = [const { core::sync::atomic::AtomicUsize::new(0) }; 64];

fn reap_detached() {
    for slot in DETACHED.iter() {
        let p = slot.load(Ordering::Acquire);
        if p == 0 {
            continue;
        }
        let st = p as *mut tls::ThreadState;
        let done = unsafe { &*(core::ptr::addr_of!((*st).done) as *const AtomicU32) };
        if sys::thread_done(done.load(Ordering::Acquire)) && slot.compare_exchange(p, 0, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
            unsafe { thread::Thread::from_raw(st).join() };
        }
    }
}

pub unsafe extern "C" fn pthread_create(t: *mut usize, attr: *const c_void, f: Start, arg: *mut c_void) -> i32 {
    reap_detached();
    let (detached, stack) = if attr.is_null() {
        (false, 0)
    } else {
        let a = &*(attr as *const Attr);
        (a.detached != 0, a.stack)
    };
    let stack = if stack == 0 { STACK } else { stack };
    let b = alloc::boxed::Box::into_raw(alloc::boxed::Box::new(Start2 { f, arg }));
    match thread::spawn(start_wrapper, b as usize, stack) {
        Ok(th) => {
            let raw = th.into_raw();
            *t = raw as usize;
            if detached {
                match DETACHED.iter().find(|s| s.compare_exchange(0, raw as usize, Ordering::AcqRel, Ordering::Relaxed).is_ok()) {
                    Some(_) => {}
                    // Full: the thread's stack and control block stay allocated (logged).
                    None => hospes_rt::hlog!("hospes-glibc: detached-thread table full; one thread's stack leaks"),
                }
            }
            0
        }
        Err(_) => {
            drop(alloc::boxed::Box::from_raw(b));
            abi::EAGAIN
        }
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
pub unsafe extern "C" fn pthread_attr_setdetachstate(a: *mut u8, s: i32) -> i32 {
    // PTHREAD_CREATE_JOINABLE 0, PTHREAD_CREATE_DETACHED 1
    if !(0..=1).contains(&s) {
        return abi::EINVAL;
    }
    (*(a as *mut Attr)).detached = s as u32;
    0
}
pub unsafe extern "C" fn pthread_attr_setstacksize(a: *mut u8, n: usize) -> i32 {
    if n < 16384 {
        return abi::EINVAL;
    }
    (*(a as *mut Attr)).stack = n;
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
pub unsafe extern "C" fn pthread_mutex_trylock(m: *mut u8) -> i32 {
    match word(m).compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed) {
        Ok(_) => 0,
        Err(_) => abi::EBUSY,
    }
}
pub unsafe extern "C" fn pthread_mutexattr_init(a: *mut u8) -> i32 {
    core::ptr::write_bytes(a, 0, abi::SIZEOF_PTHREAD_MUTEXATTR_T);
    0
}
pub extern "C" fn pthread_mutexattr_destroy(_a: *mut u8) -> i32 {
    0
}
pub unsafe extern "C" fn pthread_mutex_unlock(m: *mut u8) -> i32 {
    let w = word(m);
    if w.swap(0, Ordering::Release) == 2 {
        sys::futex_wake(w.as_ptr(), 1);
    }
    0
}

// ---- condition variables: a sequence counter ---------------------------------------------

/// The second word of a condition variable holds its clock (0 REALTIME, 1 MONOTONIC),
/// copied from the attribute at init.
pub unsafe extern "C" fn pthread_cond_init(c: *mut u8, attr: *const u32) -> i32 {
    core::ptr::write_bytes(c, 0, abi::SIZEOF_PTHREAD_COND_T);
    if !attr.is_null() {
        *(c as *mut u32).add(1) = *attr;
    }
    0
}
pub unsafe extern "C" fn pthread_condattr_init(a: *mut u32) -> i32 {
    core::ptr::write_bytes(a as *mut u8, 0, abi::SIZEOF_PTHREAD_CONDATTR_T);
    0
}
pub extern "C" fn pthread_condattr_destroy(_a: *mut u32) -> i32 {
    0
}
pub unsafe extern "C" fn pthread_condattr_setclock(a: *mut u32, clock: i32) -> i32 {
    match clock {
        0 | 1 => {
            *a = clock as u32;
            0
        }
        _ => abi::EINVAL,
    }
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
    if *(c as *const u32).add(1) == 1 {
        // A monotonic deadline: wait the same distance against the realtime clock.
        let now = sys::monotonic_ns() as i128;
        let want = (*t).sec as i128 * 1_000_000_000 + (*t).nsec as i128;
        let (rs, rns) = sys::realtime();
        let rt = rs as i128 * 1_000_000_000 + rns as i128 + (want - now).max(0);
        let d = Timespec { sec: (rt / 1_000_000_000) as i64, nsec: (rt % 1_000_000_000) as i64 };
        return cond_wait(c, m, Some(&d));
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

// ---- once, read-write locks ---------------------------------------------------------------

/// `pthread_once_t` is an int, 0 initially: 0 not run, 1 running, 2 done.
pub unsafe extern "C" fn pthread_once(o: *mut i32, f: extern "C" fn()) -> i32 {
    let w = word(o);
    loop {
        match w.compare_exchange(0, 1, Ordering::Acquire, Ordering::Acquire) {
            Ok(_) => {
                f();
                w.store(2, Ordering::Release);
                sys::futex_wake(w.as_ptr(), i32::MAX as u32);
                return 0;
            }
            Err(2) => return 0,
            Err(v) => sys::futex_wait(w.as_ptr(), v),
        }
    }
}

/// First word: the number of readers, or `WRITER`. Waiters sleep on the word and every
/// unlock wakes them all; the CDM's locks are lightly contended.
const WRITER: u32 = u32::MAX;

pub unsafe extern "C" fn pthread_rwlock_init(l: *mut u8, _attr: *const c_void) -> i32 {
    core::ptr::write_bytes(l, 0, abi::SIZEOF_PTHREAD_RWLOCK_T);
    0
}
pub extern "C" fn pthread_rwlock_destroy(_l: *mut u8) -> i32 {
    0
}
pub unsafe extern "C" fn pthread_rwlock_rdlock(l: *mut u8) -> i32 {
    let w = word(l);
    loop {
        let v = w.load(Ordering::Acquire);
        if v == WRITER || v == WRITER - 1 {
            sys::futex_wait(w.as_ptr(), v);
        } else if w.compare_exchange_weak(v, v + 1, Ordering::Acquire, Ordering::Relaxed).is_ok() {
            return 0;
        }
    }
}
pub unsafe extern "C" fn pthread_rwlock_wrlock(l: *mut u8) -> i32 {
    let w = word(l);
    loop {
        match w.compare_exchange(0, WRITER, Ordering::Acquire, Ordering::Relaxed) {
            Ok(_) => return 0,
            Err(v) => sys::futex_wait(w.as_ptr(), v),
        }
    }
}
pub unsafe extern "C" fn pthread_rwlock_unlock(l: *mut u8) -> i32 {
    let w = word(l);
    let v = w.load(Ordering::Acquire);
    if v == WRITER {
        w.store(0, Ordering::Release);
    } else if v > 0 {
        w.fetch_sub(1, Ordering::Release);
    } else {
        return abi::EPERM;
    }
    sys::futex_wake(w.as_ptr(), i32::MAX as u32);
    0
}

// ---- thread-specific data, names, scheduling ------------------------------------------------

/// Keys live in `ThreadState::shim[0..KEYS]`; the names in `shim[NAME..NAME + 2]`.
const KEYS: usize = 24;
const NAME: usize = 24;
static NEXT_KEY: AtomicU32 = AtomicU32::new(0);
static DTORS: [core::sync::atomic::AtomicUsize; KEYS] = [const { core::sync::atomic::AtomicUsize::new(0) }; KEYS];

pub unsafe extern "C" fn pthread_key_create(k: *mut u32, dtor: Option<extern "C" fn(*mut c_void)>) -> i32 {
    let n = NEXT_KEY.fetch_add(1, Ordering::AcqRel);
    if n as usize >= KEYS {
        NEXT_KEY.store(KEYS as u32, Ordering::Release);
        return abi::EAGAIN;
    }
    DTORS[n as usize].store(dtor.map_or(0, |f| f as usize), Ordering::Release);
    *k = n;
    0
}
pub unsafe extern "C" fn pthread_getspecific(k: u32) -> *mut c_void {
    if k as usize >= KEYS {
        return core::ptr::null_mut();
    }
    (*tls::current()).shim[k as usize] as *mut c_void
}
pub unsafe extern "C" fn pthread_setspecific(k: u32, v: *const c_void) -> i32 {
    if k as usize >= KEYS || k >= NEXT_KEY.load(Ordering::Acquire) {
        return abi::EINVAL;
    }
    (*tls::current()).shim[k as usize] = v as usize;
    0
}
unsafe fn run_key_destructors() {
    let st = tls::current();
    for _ in 0..4 {
        let mut any = false;
        for k in 0..KEYS {
            let v = (*st).shim[k];
            let d = DTORS[k].load(Ordering::Acquire);
            if v != 0 && d != 0 {
                (*st).shim[k] = 0;
                let f: extern "C" fn(*mut c_void) = core::mem::transmute(d);
                f(v as *mut c_void);
                any = true;
            }
        }
        if !any {
            break;
        }
    }
}

pub unsafe extern "C" fn pthread_setname_np(t: usize, name: *const u8) -> i32 {
    let n = crate::mem::strlen(name);
    if n > 15 {
        return abi::ERANGE;
    }
    let st = t as *mut tls::ThreadState;
    let dst = core::ptr::addr_of_mut!((*st).shim[NAME]) as *mut u8;
    core::ptr::write_bytes(dst, 0, 16);
    core::ptr::copy_nonoverlapping(name, dst, n);
    0
}
pub unsafe extern "C" fn pthread_getname_np(t: usize, buf: *mut u8, len: usize) -> i32 {
    let st = t as *mut tls::ThreadState;
    let src = core::ptr::addr_of!((*st).shim[NAME]) as *const u8;
    let n = crate::mem::strlen(src).min(15);
    if len < n + 1 {
        return abi::ERANGE;
    }
    core::ptr::copy_nonoverlapping(src, buf, n);
    *buf.add(n) = 0;
    0
}
/// One CPU, as `sched_getaffinity` reports.
pub unsafe extern "C" fn pthread_getaffinity_np(_t: usize, size: usize, mask: *mut u8) -> i32 {
    if crate::misc::sched_getaffinity(0, size, mask) == 0 {
        0
    } else {
        abi::EINVAL
    }
}
/// SCHED_OTHER, priority 0.
pub unsafe extern "C" fn pthread_getschedparam(_t: usize, policy: *mut i32, param: *mut i32) -> i32 {
    if !policy.is_null() {
        *policy = 0;
    }
    if !param.is_null() {
        *param = 0;
    }
    0
}
