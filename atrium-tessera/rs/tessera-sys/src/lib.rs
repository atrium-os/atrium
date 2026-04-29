//! Raw FFI bindings to libtessera_core.
//!
//! Subset that the Phase-2 tools (mkfs-tessera, tessera-debug) need
//! plus the long-standing error / hash bindings. Future phases will
//! grow this; for now it is hand-written rather than bindgen-generated
//! so the public surface stays explicit and reviewable.

#![allow(non_camel_case_types)]

use core::ffi::{c_int, c_void};

/* ── error codes ─────────────────────────────────────────────── */

pub type tessera_errno_t = c_int;

pub const TESSERA_OK:           tessera_errno_t =   0;
pub const TESSERA_EINVAL:       tessera_errno_t =  -1;
pub const TESSERA_ENOMEM:       tessera_errno_t =  -2;
pub const TESSERA_ENOTIMPL:     tessera_errno_t =  -3;
pub const TESSERA_EIO:          tessera_errno_t =  -4;
pub const TESSERA_ENOSPC:       tessera_errno_t =  -5;
pub const TESSERA_ENOENT:       tessera_errno_t =  -6;
pub const TESSERA_EEXIST:       tessera_errno_t =  -7;
pub const TESSERA_EBADMAGIC:    tessera_errno_t =  -8;
pub const TESSERA_EBADCRC:      tessera_errno_t =  -9;
pub const TESSERA_EBADHASH:     tessera_errno_t = -10;
pub const TESSERA_EBADVERSION:  tessera_errno_t = -11;
pub const TESSERA_EINCOMPAT:    tessera_errno_t = -12;
pub const TESSERA_ETOOBIG:      tessera_errno_t = -13;
pub const TESSERA_ECORRUPT:     tessera_errno_t = -14;

extern "C" {
    pub fn tessera_strerror(e: tessera_errno_t) -> *const core::ffi::c_char;

    /* SHA-256 (FreeBSD libmd, HW-accelerated where available). */
    pub fn tessera_sha256(data: *const u8, len: usize, out: *mut u8);
    pub fn tessera_hash_equal(a: *const u8, b: *const u8) -> c_int;
    pub fn tessera_hash_is_null(h: *const u8) -> c_int;
}

/* ── block I/O vtable ────────────────────────────────────────── */

#[repr(C)]
pub struct tessera_block_io_t {
    pub read_block:  Option<extern "C" fn(*mut c_void, u64, *mut u8) -> c_int>,
    pub write_block: Option<extern "C" fn(*mut c_void, u64, *const u8) -> c_int>,
    pub alloc:       Option<extern "C" fn(*mut c_void, u64, *mut u64) -> c_int>,
    pub free:        Option<extern "C" fn(*mut c_void, u64, u64) -> c_int>,
    pub ctx:         *mut c_void,
}

/* ── volume layer ────────────────────────────────────────────── */

#[repr(C)]
pub struct tessera_volume_t {
    _opaque: [u8; 0],
}

#[repr(C)]
pub struct tessera_format_opts_t {
    pub total_sectors:   u64,
    pub journal_sectors: u64,
    pub volume_uuid:     [u8; 16],
}

/* ── extent allocator ────────────────────────────────────────── */

#[repr(C)]
pub struct tessera_extent_alloc_t { _opaque: [u8; 0] }

extern "C" {
    pub fn tessera_extent_open(io:   *const tessera_block_io_t,
                                root: u64) -> *mut tessera_extent_alloc_t;
    pub fn tessera_extent_close(a: *mut tessera_extent_alloc_t);
    pub fn tessera_extent_alloc(a: *mut tessera_extent_alloc_t,
                                 n: u64, out_start: *mut u64) -> c_int;
    pub fn tessera_extent_free(a: *mut tessera_extent_alloc_t,
                                start: u64, n: u64) -> c_int;
    pub fn tessera_extent_free_blocks      (a: *const tessera_extent_alloc_t) -> u64;
    pub fn tessera_extent_largest_free_run (a: *const tessera_extent_alloc_t) -> u64;
    pub fn tessera_extent_flush(a: *mut tessera_extent_alloc_t,
                                 out_root: *mut u64) -> c_int;
}

/* ── journal ─────────────────────────────────────────────────── */

pub type tessera_record_type_t = c_int;
pub const TESSERA_TX_BEGIN:         tessera_record_type_t = 1;
pub const TESSERA_TX_COMMIT:        tessera_record_type_t = 2;
pub const TESSERA_TX_ABORT:         tessera_record_type_t = 3;
pub const TESSERA_INODE_WRITE:      tessera_record_type_t = 4;
pub const TESSERA_INODE_FREE:       tessera_record_type_t = 5;
pub const TESSERA_MANIFEST_REPOINT: tessera_record_type_t = 6;
pub const TESSERA_DIR_INSERT:       tessera_record_type_t = 7;
pub const TESSERA_DIR_REMOVE:       tessera_record_type_t = 8;
pub const TESSERA_PACK_PUBLISH:     tessera_record_type_t = 9;
pub const TESSERA_PACK_RETIRE:      tessera_record_type_t = 10;

#[repr(C)]
pub struct tessera_journal_t { _opaque: [u8; 0] }

#[repr(C)]
pub struct tessera_record_header_t {
    pub magic:        [u8; 4],
    pub record_type:  u32,
    pub sequence:     u64,
    pub body_length:  u32,
    pub block_count:  u32,
    pub crc32_body:   u32,
    pub crc32_header: u32,
}

pub type tessera_replay_cb_t = extern "C" fn(
    ctx:  *mut c_void,
    hdr:  *const tessera_record_header_t,
    body: *const u8,
) -> c_int;

extern "C" {
    pub fn tessera_journal_format(io: *const tessera_block_io_t,
                                   start: u64, length: u64) -> c_int;
    pub fn tessera_journal_open(io: *const tessera_block_io_t,
                                 start: u64, length: u64)
                                 -> *mut tessera_journal_t;
    pub fn tessera_journal_close(j: *mut tessera_journal_t);

    pub fn tessera_journal_tx_begin(j: *mut tessera_journal_t,
                                     out_tx_id: *mut u64,
                                     reason_tag: *const u8) -> c_int;
    pub fn tessera_journal_append(j: *mut tessera_journal_t, tx_id: u64,
                                   record_type: tessera_record_type_t,
                                   body: *const u8, body_len: u32) -> c_int;
    pub fn tessera_journal_tx_commit(j: *mut tessera_journal_t,
                                      tx_id: u64) -> c_int;
    pub fn tessera_journal_tx_abort(j: *mut tessera_journal_t, tx_id: u64,
                                     reason: u32) -> c_int;
    pub fn tessera_journal_replay(j: *mut tessera_journal_t,
                                   cb: tessera_replay_cb_t,
                                   ctx: *mut c_void) -> c_int;
}

/* ── volume ──────────────────────────────────────────────────── */

extern "C" {
    pub fn tessera_volume_format(io:   *const tessera_block_io_t,
                                 opts: *const tessera_format_opts_t) -> c_int;

    pub fn tessera_volume_open(io:  *const tessera_block_io_t,
                               out: *mut *mut tessera_volume_t) -> c_int;

    pub fn tessera_volume_close(v: *mut tessera_volume_t);

    pub fn tessera_volume_total_sectors    (v: *const tessera_volume_t) -> u64;
    pub fn tessera_volume_generation       (v: *const tessera_volume_t) -> u64;
    pub fn tessera_volume_inode_root       (v: *const tessera_volume_t) -> u64;
    pub fn tessera_volume_pack_registry_root(v: *const tessera_volume_t) -> u64;
    pub fn tessera_volume_free_extent_root (v: *const tessera_volume_t) -> u64;
    pub fn tessera_volume_journal_start    (v: *const tessera_volume_t) -> u64;
    pub fn tessera_volume_journal_length   (v: *const tessera_volume_t) -> u64;
    pub fn tessera_volume_uuid             (v: *const tessera_volume_t) -> *const u8;
}
