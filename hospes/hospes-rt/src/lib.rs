//! `hospes-rt`: the `no_std`, libc-free runtime and loader for Hospes
//! (docs/spec/hospes.md §4–§5).
//!
//! A Hospes host is a static binary with our own `_start` ([`start`]). It owns the thread
//! pointer outright (nothing of ours uses TLS), so it can give every thread the
//! glibc-shaped control block a foreign library expects ([`tls`]), map and relocate that
//! library ([`load`]) and run its threads ([`thread`]). Memory comes from our own allocator
//! ([`heap`]) over `mmap`. System calls are raw ([`sys`]): FreeBSD in production, Linux as a
//! development backend.
#![no_std]
#![allow(clippy::missing_safety_doc)]

extern crate alloc;

pub mod heap;
pub mod load;
pub mod lock;
pub mod log;
pub mod start;
pub mod sys;
pub mod thread;
pub mod tls;

#[cfg(feature = "global")]
#[global_allocator]
static GLOBAL: heap::Heap = heap::Heap;

#[cfg(feature = "global")]
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    let _ = core::fmt::Write::write_fmt(&mut log::Stderr, format_args!("hospes: panic: {info}\n"));
    sys::exit(101)
}
