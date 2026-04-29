//! Raw FFI bindings to libtessera_core.
//!
//! Phase 0: only the error namespace and the SHA-256 helper are bound.
//! Phase 1 expands this with the codec, CDC, B+tree, manifest, pack,
//! journal, extent, and GC entry points.

#![allow(non_camel_case_types)]

use core::ffi::{c_char, c_int};

pub type tessera_errno_t = c_int;

pub const TESSERA_OK:        tessera_errno_t =  0;
pub const TESSERA_EINVAL:    tessera_errno_t = -1;
pub const TESSERA_ENOMEM:    tessera_errno_t = -2;
pub const TESSERA_EIO:       tessera_errno_t = -3;
pub const TESSERA_ECORRUPT:  tessera_errno_t = -4;
pub const TESSERA_ENOTIMPL:  tessera_errno_t = -22;

pub type tessera_hash_t = [u8; 32];

extern "C" {
    pub fn tessera_strerror(e: tessera_errno_t) -> *const c_char;

    pub fn tessera_sha256(
        data: *const u8,
        len:  usize,
        out:  *mut u8, // tessera_hash_t
    );

    pub fn tessera_hash_equal(a: *const u8, b: *const u8) -> c_int;
    pub fn tessera_hash_is_null(h: *const u8) -> c_int;
}
