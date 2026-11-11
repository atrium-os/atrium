//! The pieces of the C++ runtime the libraries import.
//!
//! No unwinder: OpenH264 imports neither `__cxa_throw` nor `__cxa_allocate_exception`, so the
//! only way an exception could start is our `operator new` failing, and ours ends the
//! process instead of throwing `std::bad_alloc`. The personality routine and
//! `_Unwind_Resume` are therefore never reached; they end the process if they are.
use crate::die;
use core::sync::atomic::{AtomicU32, AtomicU8, Ordering};
use hospes_rt::{heap, sys};

pub unsafe extern "C" fn operator_new(n: usize) -> *mut u8 {
    let p = heap::alloc(n, 16);
    if p.is_null() {
        die("operator new: out of memory");
    }
    p
}
pub unsafe extern "C" fn operator_delete(p: *mut u8) {
    heap::free(p)
}
pub unsafe extern "C" fn operator_delete_sized(p: *mut u8, _n: usize) {
    heap::free(p)
}

// Itanium guard object: byte 0 says "initialised" (inline code tests it). We use the second
// 32-bit word as the in-progress state: 0 idle, 1 busy, 2 busy with waiters.
unsafe fn parts(g: *mut u64) -> (&'static AtomicU8, &'static AtomicU32) {
    (&*(g as *const AtomicU8), &*((g as *mut u32).add(1) as *const AtomicU32))
}

pub unsafe extern "C" fn __cxa_guard_acquire(g: *mut u64) -> i32 {
    let (done, st) = parts(g);
    loop {
        if done.load(Ordering::Acquire) != 0 {
            return 0;
        }
        if st.compare_exchange(0, 1, Ordering::Acquire, Ordering::Acquire).is_ok() {
            if done.load(Ordering::Acquire) != 0 {
                st.store(0, Ordering::Release);
                return 0;
            }
            return 1;
        }
        let _ = st.compare_exchange(1, 2, Ordering::Acquire, Ordering::Acquire);
        sys::futex_wait(st.as_ptr(), 2);
    }
}
pub unsafe extern "C" fn __cxa_guard_release(g: *mut u64) {
    let (done, st) = parts(g);
    done.store(1, Ordering::Release);
    if st.swap(0, Ordering::Release) == 2 {
        sys::futex_wake(st.as_ptr(), u32::MAX >> 1);
    }
}
pub unsafe extern "C" fn __cxa_guard_abort(g: *mut u64) {
    let (_, st) = parts(g);
    if st.swap(0, Ordering::Release) == 2 {
        sys::futex_wake(st.as_ptr(), u32::MAX >> 1);
    }
}

pub extern "C" fn __cxa_pure_virtual() -> ! {
    die("pure virtual function called")
}
pub extern "C" fn __cxa_throw_bad_array_new_length() -> ! {
    die("bad array new length")
}
pub extern "C" fn __gxx_personality_v0() -> ! {
    die("C++ exception unwinding reached (not supported)")
}
pub extern "C" fn _Unwind_Resume() -> ! {
    die("_Unwind_Resume reached (not supported)")
}

extern "C" fn type_info_method() -> ! {
    die("RTTI type_info method called (dynamic_cast / exceptions are not supported)")
}

/// `__cxxabiv1::__class_type_info` and friends' vtables. Type-info objects in the library
/// point at `vtable + 16`; only `dynamic_cast` and exception matching would call through
/// them, and the libraries import neither.
#[repr(C)]
pub struct Vtable([usize; 16]);
const fn vt() -> Vtable {
    // [offset-to-top, typeinfo, methods...]: methods are filled at first use below.
    Vtable([0; 16])
}
pub static mut CLASS_TYPE_INFO_VTABLE: Vtable = vt();
pub static mut SI_CLASS_TYPE_INFO_VTABLE: Vtable = vt();
pub static mut VMI_CLASS_TYPE_INFO_VTABLE: Vtable = vt();

/// Point every virtual slot at a function that ends the process.
pub fn init_vtables() {
    let f = type_info_method as *const () as usize;
    unsafe {
        for v in [
            core::ptr::addr_of_mut!(CLASS_TYPE_INFO_VTABLE),
            core::ptr::addr_of_mut!(SI_CLASS_TYPE_INFO_VTABLE),
            core::ptr::addr_of_mut!(VMI_CLASS_TYPE_INFO_VTABLE),
        ] {
            for i in 2..16 {
                (*v).0[i] = f;
            }
        }
    }
}
