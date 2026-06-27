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
    pub total_sectors:        u64,
    pub journal_sectors:      u64,
    pub volume_uuid:          [u8; 16],
    pub seed_dirent_name:     *const u8,    /* NULL = no seed */
    pub seed_dirent_name_len: u16,
    pub seed_dirent_inode:    u64,
    pub seed_content_data:    *const u8,    /* NULL = empty file */
    pub seed_content_len:     usize,
    pub seed_chunk_size:      u32,          /* 0 = INLINE, >0 = CHUNK_LIST */
}

/* ── B+tree ──────────────────────────────────────────────────── */

#[repr(C)]
pub struct tessera_btree_t        { _opaque: [u8; 0] }
#[repr(C)]
pub struct tessera_btree_cursor_t { _opaque: [u8; 0] }

/* tree kinds + fixed record sizes (format.h) */
pub const TESSERA_BTREE_KIND_INODE:    u8 = 0;
pub const TESSERA_BTREE_KIND_PACK_REG: u8 = 1;
pub const TESSERA_BTREE_KIND_FREE_EXT: u8 = 2;
pub const TESSERA_BTREE_KIND_SNAPSHOT: u8 = 3;
pub const TESSERA_BTREE_KIND_QUOTA:    u8 = 4;

pub const TESSERA_INODE_RECORD_SIZE:   u32 = 144;
pub const TESSERA_REGISTRY_ENTRY_SIZE: u32 = 64;

pub const TESSERA_REGISTRY_FLAG_SEALED:       u32 = 1 << 0;
pub const TESSERA_REGISTRY_FLAG_RETIRING:     u32 = 1 << 1;
pub const TESSERA_REGISTRY_FLAG_MULTI_EXTENT: u32 = 1 << 2;

extern "C" {
    pub fn tessera_btree_create(io: *const tessera_block_io_t,
                                 tree_kind:  u8,
                                 key_size:   u32,
                                 value_size: u32,
                                 out_root_sector: *mut u64)
                                 -> *mut tessera_btree_t;
    pub fn tessera_btree_open  (io: *const tessera_block_io_t,
                                 root_sector: u64,
                                 tree_kind:   u8,
                                 key_size:    u32,
                                 value_size:  u32) -> *mut tessera_btree_t;
    pub fn tessera_btree_close (t: *mut tessera_btree_t);

    pub fn tessera_btree_get   (t: *mut tessera_btree_t,
                                 key: *const u8, out_value: *mut u8) -> c_int;
    pub fn tessera_btree_put   (t: *mut tessera_btree_t,
                                 key: *const u8, value: *const u8,
                                 out_new_root: *mut u64) -> c_int;
    pub fn tessera_btree_delete(t: *mut tessera_btree_t,
                                 key: *const u8,
                                 out_new_root: *mut u64) -> c_int;

    pub fn tessera_btree_seek_first(t: *mut tessera_btree_t)
                                     -> *mut tessera_btree_cursor_t;
    pub fn tessera_btree_cursor_get (c: *mut tessera_btree_cursor_t,
                                      out_key: *mut u8,
                                      out_value: *mut u8) -> c_int;
    pub fn tessera_btree_cursor_next(c: *mut tessera_btree_cursor_t) -> c_int;
    pub fn tessera_btree_cursor_free(c: *mut tessera_btree_cursor_t);
}

/* ── pack files ──────────────────────────────────────────────── */

pub const TESSERA_BLOB_FLAG_MANIFEST: u32 = 1 << 0;
pub const TESSERA_BLOB_FLAG_CHUNK:    u32 = 1 << 1;

#[repr(C)]
pub struct tessera_pack_builder_t { _opaque: [u8; 0] }
#[repr(C)]
pub struct tessera_pack_reader_t  { _opaque: [u8; 0] }

extern "C" {
    pub fn tessera_pack_begin(pack_kind: u32,
                               pack_id: *const u8,
                               creator_tx_id: u64) -> *mut tessera_pack_builder_t;
    pub fn tessera_pack_add_blob(b: *mut tessera_pack_builder_t,
                                  blob_hash: *const u8,
                                  bytes: *const u8, len: u32,
                                  flags: u32) -> c_int;
    pub fn tessera_pack_finalize(b: *mut tessera_pack_builder_t,
                                  out_buf: *mut u8, buf_len: usize,
                                  out_size: *mut usize) -> c_int;
    pub fn tessera_pack_free(b: *mut tessera_pack_builder_t);

    pub fn tessera_pack_open(data: *const u8, len: usize)
                              -> *mut tessera_pack_reader_t;
    pub fn tessera_pack_blob_count(r: *const tessera_pack_reader_t) -> u32;
    pub fn tessera_pack_blob_hash_at(r: *const tessera_pack_reader_t,
                                      index: u32, out: *mut u8) -> c_int;
    pub fn tessera_pack_lookup(r: *const tessera_pack_reader_t,
                                blob_hash: *const u8,
                                out_bytes: *mut *const u8,
                                out_len: *mut u32) -> c_int;
    pub fn tessera_pack_close(r: *mut tessera_pack_reader_t);
}

/* ── content-defined chunking ────────────────────────────────── */

#[repr(C)]
pub struct tessera_cdc_params_t {
    pub avg_chunk: u32,
    pub min_chunk: u32,
    pub max_chunk: u32,
}

extern "C" {
    pub static tessera_cdc_default_params: tessera_cdc_params_t;

    pub fn tessera_cdc_split(data: *const u8, len: usize,
                              params: *const tessera_cdc_params_t,
                              out_boundaries: *mut usize,
                              cap: usize, n_out: *mut usize) -> c_int;
}

/* ── manifests ───────────────────────────────────────────────── */

pub type tessera_manifest_kind_t = c_int;
pub const TESSERA_MFT_INLINE:          tessera_manifest_kind_t = 1;
pub const TESSERA_MFT_CHUNK_LIST:      tessera_manifest_kind_t = 2;
pub const TESSERA_MFT_CHUNK_TREE:      tessera_manifest_kind_t = 3;
pub const TESSERA_MFT_DIRECTORY:       tessera_manifest_kind_t = 4;
pub const TESSERA_MFT_SYMLINK:         tessera_manifest_kind_t = 5;
pub const TESSERA_MFT_XATTR_STORE:     tessera_manifest_kind_t = 6;
pub const TESSERA_MFT_GC_ROOT_LIST:    tessera_manifest_kind_t = 7;
pub const TESSERA_MFT_DIRECTORY_2L:    tessera_manifest_kind_t = 8;
pub const TESSERA_MFT_DIRECTORY_BTREE: tessera_manifest_kind_t = 9;

#[repr(C)]
pub struct tessera_manifest_builder_t { _opaque: [u8; 0] }
#[repr(C)]
pub struct tessera_manifest_parser_t  { _opaque: [u8; 0] }

#[repr(C)]
pub struct tessera_chunk_record_t {
    pub chunk_hash:        [u8; 32],
    pub logical_offset:    u64,
    pub uncompressed_size: u32,
    pub flags:             u32,
}

#[repr(C)]
pub struct tessera_tree_record_t {
    pub child_manifest_hash: [u8; 32],
    pub logical_offset:      u64,
}

#[repr(C)]
pub struct tessera_dir_bucket_record_t {
    pub first_name_hash:      u64,
    pub bucket_manifest_hash: [u8; 32],
}

extern "C" {
    pub fn tessera_manifest_begin(kind: tessera_manifest_kind_t)
                                   -> *mut tessera_manifest_builder_t;
    pub fn tessera_manifest_add_chunk(b: *mut tessera_manifest_builder_t,
                                       chunk_hash: *const u8,
                                       logical_offset: u64,
                                       size: u32, flags: u32) -> c_int;
    pub fn tessera_manifest_set_inline(b: *mut tessera_manifest_builder_t,
                                        data: *const u8, len: usize) -> c_int;
    pub fn tessera_manifest_finalize(b: *mut tessera_manifest_builder_t,
                                      out_buf: *mut u8, buf_len: usize,
                                      out_size: *mut usize,
                                      out_hash: *mut u8) -> c_int;
    pub fn tessera_manifest_free(b: *mut tessera_manifest_builder_t);

    pub fn tessera_manifest_parse(data: *const u8, len: usize)
                                   -> *mut tessera_manifest_parser_t;
    pub fn tessera_manifest_parser_kind (p: *const tessera_manifest_parser_t)
                                          -> tessera_manifest_kind_t;
    pub fn tessera_manifest_parser_size (p: *const tessera_manifest_parser_t) -> u64;
    pub fn tessera_manifest_parser_count(p: *const tessera_manifest_parser_t) -> u32;
    pub fn tessera_manifest_chunk_at(p: *const tessera_manifest_parser_t,
                                      index: u32,
                                      out: *mut tessera_chunk_record_t) -> c_int;
    pub fn tessera_manifest_tree_at(p: *const tessera_manifest_parser_t,
                                     index: u32,
                                     out: *mut tessera_tree_record_t) -> c_int;
    pub fn tessera_manifest_dir_bucket_at(p: *const tessera_manifest_parser_t,
                                           index: u32,
                                           out: *mut tessera_dir_bucket_record_t)
                                           -> c_int;
    pub fn tessera_manifest_inline_data(p: *const tessera_manifest_parser_t,
                                         out_data: *mut *const u8,
                                         out_len: *mut usize) -> c_int;
    pub fn tessera_manifest_parser_free(p: *mut tessera_manifest_parser_t);
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

    /* v2 fields (added 2026-04-30). */
    pub fn tessera_volume_snapshots_root   (v: *const tessera_volume_t) -> u64;
    pub fn tessera_volume_snapshots_gen    (v: *const tessera_volume_t) -> u64;
    pub fn tessera_volume_meta_reserve_start (v: *const tessera_volume_t) -> u64;
    pub fn tessera_volume_meta_reserve_length(v: *const tessera_volume_t) -> u64;
    pub fn tessera_volume_meta_reserve_bump  (v: *const tessera_volume_t) -> u64;
    pub fn tessera_volume_pack_zone_start    (v: *const tessera_volume_t) -> u64;
    pub fn tessera_volume_pack_zone_length   (v: *const tessera_volume_t) -> u64;
    pub fn tessera_volume_encryption_flags   (v: *const tessera_volume_t) -> u16;
    pub fn tessera_volume_active_slot_count  (v: *const tessera_volume_t) -> u8;
}
