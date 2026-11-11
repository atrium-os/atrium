//! A small futex mutex (0 free, 1 held, 2 held with waiters), on `_umtx_op` / `futex`.
use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicU32, Ordering};

pub struct Mutex<T> {
    state: AtomicU32,
    v: UnsafeCell<T>,
}

unsafe impl<T: Send> Sync for Mutex<T> {}
unsafe impl<T: Send> Send for Mutex<T> {}

pub struct Guard<'a, T> {
    m: &'a Mutex<T>,
}

impl<T> Mutex<T> {
    pub const fn new(v: T) -> Self {
        Mutex { state: AtomicU32::new(0), v: UnsafeCell::new(v) }
    }
    pub fn lock(&self) -> Guard<'_, T> {
        if self.state.compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed).is_err() {
            self.slow();
        }
        Guard { m: self }
    }
    #[cold]
    fn slow(&self) {
        for _ in 0..100 {
            if self.state.compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed).is_ok() {
                return;
            }
            core::hint::spin_loop();
        }
        while self.state.swap(2, Ordering::Acquire) != 0 {
            unsafe { crate::sys::futex_wait(self.state.as_ptr(), 2) };
        }
    }
}

impl<T> Drop for Guard<'_, T> {
    fn drop(&mut self) {
        if self.m.state.swap(0, Ordering::Release) == 2 {
            unsafe { crate::sys::futex_wake(self.m.state.as_ptr(), 1) };
        }
    }
}

impl<T> Deref for Guard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.m.v.get() }
    }
}
impl<T> DerefMut for Guard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.m.v.get() }
    }
}
