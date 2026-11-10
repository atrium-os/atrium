/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * tessera_decode.rs -- crate root of tessera_fs.ko's Rust part: Tessera's
 * on-disk decoders (kernel security review 2026-11-07, decision D3).
 *
 * The code lives in ../rs/tessera-decode/src, shared with userspace, and is
 * named here module by module so that freebsd-kpi-rs's kmod.rust.mk (which
 * compiles one .rs file per RUST_SRCS entry) builds the same sources cargo
 * does. Keep this list in step with ../rs/tessera-decode/src/lib.rs.
 *
 * The C side (tessera_fs.c and the tessera-core files in this module, built
 * with -DTESSERA_RUST_DECODE) calls the tessera_rs_* exports in ffi.rs; see
 * ../core/src/tessera_rs.h. Nothing here allocates, locks or sleeps, and the
 * compiled code has no panic path (scripts/tessera-decode-check.sh), so the
 * KPI crate's panic handler is linked but never reached from these functions.
 */
#![no_std]
#![deny(unsafe_code)]

#[path = "../rs/tessera-decode/src/btree.rs"]
pub mod btree;
#[path = "../rs/tessera-decode/src/codec.rs"]
pub mod codec;
#[path = "../rs/tessera-decode/src/crc.rs"]
pub mod crc;
#[path = "../rs/tessera-decode/src/error.rs"]
pub mod error;
#[path = "../rs/tessera-decode/src/journal.rs"]
pub mod journal;
#[path = "../rs/tessera-decode/src/le.rs"]
pub mod le;
#[path = "../rs/tessera-decode/src/manifest.rs"]
pub mod manifest;
#[path = "../rs/tessera-decode/src/pack.rs"]
pub mod pack;
#[path = "../rs/tessera-decode/src/ffi.rs"]
pub mod ffi;
