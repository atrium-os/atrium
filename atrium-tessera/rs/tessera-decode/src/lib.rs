//! Tessera on-disk decoders (review 2026-11-07, step 4 / decision D3).
//!
//! Every function in this crate takes bytes that came off a disk image and
//! decides whether they describe a valid structure. Each one is safe Rust over
//! slices: a length, count or offset read from disk can only ever select a
//! sub-slice through `get()`, so a hostile value is an error return, never a
//! read of memory outside the buffer. No module here panics on any input (the
//! crate has no indexing that can fail, no `unwrap`, no arithmetic that can
//! overflow-trap); `scripts/tessera-decode-check.sh` checks that the compiled
//! kernel object has no reference to a panic routine.
//!
//! There are no large stack buffers (kmod audit T2): decoders validate in place
//! and the caller's output struct is written with one copy.
//!
//! The same source files are compiled three ways:
//!   - by cargo, for the Rust tools and `cargo test`;
//!   - as `tessera-decode-c` (a staticlib) linked into the userspace C core
//!     built with `-DTESSERA_RUST_DECODE`;
//!   - by freebsd-kpi-rs's `kmod.rust.mk`, into tessera_fs.ko, through the crate
//!     root `atrium-tessera/kmod/tessera_decode.rs`, which names these modules
//!     with `#[path]`. Keep the module list there in step with this one.
//!
//! The C entry points keep their names and signatures (`tessera/codec.h`,
//! `manifest.h`, `pack.h`, ...): the C functions call the `tessera_rs_*`
//! exports in [`ffi`] when the core is built with `TESSERA_RUST_DECODE`, and
//! keep their old C bodies otherwise. The C bodies stay for the loader (libsa
//! cannot link Rust yet) and as the reference for the differential test
//! (`core/fuzz/fuzz_diff.c`).
#![no_std]
#![deny(unsafe_code)]

pub mod btree;
pub mod codec;
pub mod crc;
pub mod error;
pub mod journal;
pub mod le;
pub mod manifest;
pub mod pack;

#[cfg(feature = "ffi")]
pub mod ffi;
