//! Threads for the foreign library: `thr_new` (FreeBSD) or `clone` (Linux dev), each with a
//! guarded stack and its own glibc-shaped control block (docs/spec/hospes.md §4).
use crate::sys;
use crate::tls::{self, ThreadState};
use core::sync::atomic::{AtomicU32, Ordering};

pub const DEFAULT_STACK: usize = 1 << 20;

pub type Entry = extern "C" fn(usize) -> usize;

pub struct Thread {
    state: *mut ThreadState,
    stack: usize,
    stack_len: usize,
}

unsafe impl Send for Thread {}

extern "C" fn trampoline(p: usize) {
    let st = p as *mut ThreadState;
    unsafe {
        let f: Entry = core::mem::transmute((*st).start);
        (*st).result = f((*st).arg);
        sys::thread_exit(core::ptr::addr_of_mut!((*st).done) as *mut u32)
    }
}

/// Start `f(arg)` on a new thread with a `stack_len` stack (plus one guard page).
pub fn spawn(f: Entry, arg: usize, stack_len: usize) -> sys::R<Thread> {
    let stack_len = sys::page_up(stack_len.max(64 * 1024));
    let total = stack_len + sys::PAGE;
    let stack = sys::mmap_anon(total, sys::PROT_READ | sys::PROT_WRITE)?;
    sys::mprotect(stack, sys::PAGE, sys::PROT_NONE)?;
    let tcb = match tls::new_tcb() {
        Ok(t) => t,
        Err(e) => {
            let _ = sys::munmap(stack, total);
            return Err(e);
        }
    };
    unsafe {
        (*tcb.state).start = f as *const () as usize;
        (*tcb.state).arg = arg;
        let done = core::ptr::addr_of_mut!((*tcb.state).done) as *mut u32;
        if let Err(e) = sys::thread_new(trampoline, tcb.state as usize, stack + sys::PAGE, stack_len, tcb.tp, done) {
            tls::free_tcb(tcb.state);
            let _ = sys::munmap(stack, total);
            return Err(e);
        }
    }
    Ok(Thread { state: tcb.state, stack, stack_len: total })
}

impl Thread {
    /// Wait for the thread to finish and return its result.
    pub fn join(self) -> usize {
        unsafe {
            let w = &*(core::ptr::addr_of!((*self.state).done) as *const AtomicU32);
            loop {
                let v = w.load(Ordering::Acquire);
                if sys::thread_done(v) {
                    break;
                }
                sys::futex_wait(w.as_ptr(), v);
            }
            let r = (*self.state).result;
            tls::free_tcb(self.state);
            let _ = sys::munmap(self.stack, self.stack_len);
            r
        }
    }
}
