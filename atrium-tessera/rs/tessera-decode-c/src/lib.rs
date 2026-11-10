//! Static library carrying tessera-decode's `tessera_rs_*` C ABI into the
//! userspace C core (libtessera_core built with `-DTESSERA_RUST_DECODE`, the
//! fuzzers and the differential test). The kernel module does not use this
//! crate: kmod.rust.mk compiles tessera-decode's sources directly.
#![no_std]

extern crate std;

// Linking the rlib is what brings the #[no_mangle] exports in; this keeps the
// dependency from being dropped as unused.
pub use tessera_decode::ffi;
