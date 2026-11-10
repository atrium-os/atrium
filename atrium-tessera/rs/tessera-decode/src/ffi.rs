//! C ABI over the safe decoders: the `tessera_rs_*` functions declared in
//! core/src/tessera_rs.h, called by the C entry points when the core is built
//! with `TESSERA_RUST_DECODE`.
//!
//! This is the only module with `unsafe`. All of it is the same two things:
//! turning a pointer and length that C passed in into a slice, and writing a
//! result through an out-pointer C passed in. The contract for every function
//! is the C header's: input pointers are either NULL or point to the stated
//! number of readable bytes, out-pointers are NULL or writable and aligned for
//! their type. NULL is checked here (EINVAL, as the C did); a wrong length
//! from C is the one thing Rust cannot check.
#![allow(unsafe_code)]
#![allow(clippy::missing_safety_doc)]

use crate::btree::{self, Shape};
use crate::codec::{self, BTREE_NODE_HEADER_SIZE, HASH_SIZE, MANIFEST_HEADER_SIZE, RECORD_HEADER_SIZE, SECTOR};
use crate::error::{EINVAL, OK, Result};
use crate::journal;
use crate::manifest::Manifest;
use crate::pack::{self, Geometry};
use core::ffi::{c_char, c_int};

// ---- pointer helpers ------------------------------------------------------

/// `N` bytes at `p` as an array reference; None for NULL.
unsafe fn arr<'a, const N: usize>(p: *const u8) -> Option<&'a [u8; N]> {
    // SAFETY: caller contract — p is NULL or points to N readable bytes that
    // outlive 'a ('a never escapes the exported function's body).
    unsafe { p.cast::<[u8; N]>().as_ref() }
}

/// `len` bytes at `p`; None for NULL. A zero length gives an empty slice.
unsafe fn bytes<'a>(p: *const u8, len: usize) -> Option<&'a [u8]> {
    if p.is_null() {
        return None;
    }
    // SAFETY: caller contract — p points to len readable bytes.
    Some(unsafe { core::slice::from_raw_parts(p, len) })
}

/// Store `v` through `p` unless `p` is NULL.
unsafe fn put<T>(p: *mut T, v: T) {
    if !p.is_null() {
        // SAFETY: caller contract — p is writable and aligned for T.
        unsafe { p.write(v) }
    }
}

/// Copy `N` bytes from `src` to `dst`. `ptr::copy` is memmove, so it is sound
/// even if C passes overlapping buffers.
unsafe fn copy_out<const N: usize>(src: &[u8; N], dst: *mut u8) {
    // SAFETY: caller contract — dst is N writable bytes.
    unsafe { core::ptr::copy(src.as_ptr(), dst, N) }
}

fn rc(r: Result<()>) -> c_int {
    match r {
        Ok(()) => OK,
        Err(e) => e,
    }
}

/// Validate N input bytes with `check`, then copy them to the out struct.
unsafe fn decode<const N: usize>(inp: *const u8, out: *mut u8, check: fn(&[u8; N]) -> Result<()>) -> c_int {
    // SAFETY: forwarded caller contract (N readable bytes at inp, N at out).
    let Some(src) = (unsafe { arr::<N>(inp) }) else { return EINVAL };
    if out.is_null() {
        return EINVAL;
    }
    if let Err(e) = check(src) {
        return e;
    }
    unsafe { copy_out(src, out) };
    OK
}

fn no_check<const N: usize>(_: &[u8; N]) -> Result<()> {
    Ok(())
}

// ---- codec.h --------------------------------------------------------------

macro_rules! decoder {
    ($name:ident, $n:expr, $check:expr) => {
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $name(inp: *const u8, out: *mut u8) -> c_int {
            unsafe { decode::<{ $n }>(inp, out, $check) }
        }
    };
}

decoder!(tessera_rs_decode_superblock, SECTOR, codec::superblock);
decoder!(tessera_rs_decode_journal_header, SECTOR, codec::journal_header);
decoder!(tessera_rs_decode_record_header, RECORD_HEADER_SIZE, codec::record_header);
decoder!(tessera_rs_decode_pack_header, SECTOR, codec::pack_header);
decoder!(tessera_rs_decode_pack_footer, SECTOR, codec::pack_footer);
decoder!(tessera_rs_decode_pack_index_entry, codec::PACK_INDEX_ENTRY_SIZE, no_check);
decoder!(tessera_rs_decode_blob_descriptor, codec::BLOB_DESCRIPTOR_SIZE, codec::blob_descriptor);
decoder!(tessera_rs_decode_manifest_header, MANIFEST_HEADER_SIZE, codec::manifest_header);
decoder!(tessera_rs_decode_inode, codec::INODE_RECORD_SIZE, no_check);
decoder!(tessera_rs_decode_quota_domain, codec::QUOTA_DOMAIN_SIZE, no_check);
decoder!(tessera_rs_decode_btree_node_header, BTREE_NODE_HEADER_SIZE, codec::btree_node_header);
decoder!(tessera_rs_decode_registry_entry, codec::REGISTRY_ENTRY_SIZE, no_check);
decoder!(tessera_rs_decode_pack_extent_list, SECTOR, codec::pack_extent_list);
decoder!(tessera_rs_decode_free_extent, codec::EXTENT_ENTRY_SIZE, no_check);

/// `codec::LAYOUT[i]`, or `usize::MAX` past the end (fuzz_diff.c's layout check).
#[unsafe(no_mangle)]
pub extern "C" fn tessera_rs_layout(i: usize) -> usize {
    codec::LAYOUT.get(i).copied().unwrap_or(usize::MAX)
}

// ---- manifest.h -----------------------------------------------------------

/// The parser object C keeps: the decoded header and the body slice.
unsafe fn manifest<'a>(hdr: *const u8, body: *const u8, body_len: usize) -> Option<Manifest<'a>> {
    let h = unsafe { arr::<MANIFEST_HEADER_SIZE>(hdr) }?;
    let b = if body_len == 0 { &[][..] } else { unsafe { bytes(body, body_len) }? };
    Manifest::from_parts(h, b).ok()
}

/// Checks `data` is a manifest and copies its header to `out_hdr` (32 bytes).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessera_rs_manifest_parse(data: *const u8, len: usize, out_hdr: *mut u8) -> c_int {
    let Some(d) = (unsafe { bytes(data, len) }) else { return EINVAL };
    if out_hdr.is_null() {
        return EINVAL;
    }
    match Manifest::parse(d) {
        Ok(_) => match d.get(..MANIFEST_HEADER_SIZE).and_then(|h| <&[u8; MANIFEST_HEADER_SIZE]>::try_from(h).ok()) {
            Some(h) => {
                unsafe { copy_out(h, out_hdr) };
                OK
            }
            None => EINVAL,
        },
        Err(e) => e,
    }
}

macro_rules! manifest_record {
    ($name:ident, $method:ident, $n:expr) => {
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $name(
            hdr: *const u8, body: *const u8, body_len: usize, index: u32, out: *mut u8,
        ) -> c_int {
            let Some(m) = (unsafe { manifest(hdr, body, body_len) }) else { return EINVAL };
            if out.is_null() {
                return EINVAL;
            }
            match m.$method(index).map(<&[u8; $n]>::try_from) {
                Ok(Ok(r)) => {
                    unsafe { copy_out(r, out) };
                    OK
                }
                Ok(Err(_)) => crate::error::ECORRUPT,
                Err(e) => e,
            }
        }
    };
}

manifest_record!(tessera_rs_manifest_chunk_at, chunk_at, crate::manifest::CHUNK_RECORD_SIZE);
manifest_record!(tessera_rs_manifest_tree_at, tree_at, crate::manifest::TREE_RECORD_SIZE);
manifest_record!(tessera_rs_manifest_dir_bucket_at, dir_bucket_at, crate::manifest::DIR_BUCKET_RECORD_SIZE);
manifest_record!(tessera_rs_manifest_dir_btree_inner_at, dir_btree_inner_at, HASH_SIZE);

#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessera_rs_manifest_inline_data(
    hdr: *const u8, body: *const u8, body_len: usize, out_data: *mut *const u8, out_len: *mut usize,
) -> c_int {
    let Some(m) = (unsafe { manifest(hdr, body, body_len) }) else { return EINVAL };
    if out_data.is_null() || out_len.is_null() {
        return EINVAL;
    }
    match m.inline_data() {
        Ok(d) => {
            // Hand back the caller's own body pointer (not a dangling one for
            // an empty body), as manifest.c did.
            unsafe {
                put(out_data, if d.is_empty() { body } else { d.as_ptr() });
                put(out_len, d.len());
            }
            OK
        }
        Err(e) => e,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessera_rs_manifest_xattr_at(
    hdr: *const u8, body: *const u8, body_len: usize, idx: u32,
    out_name: *mut *const c_char, out_name_len: *mut u16,
    out_value: *mut *const u8, out_value_len: *mut u16,
) -> c_int {
    let Some(m) = (unsafe { manifest(hdr, body, body_len) }) else { return EINVAL };
    match m.xattr_at(idx) {
        Ok(x) => {
            unsafe {
                put(out_name, x.name.as_ptr().cast::<c_char>());
                put(out_name_len, x.name.len() as u16);
                put(out_value, x.val.as_ptr());
                put(out_value_len, x.val.len() as u16);
            }
            OK
        }
        Err(e) => e,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessera_rs_manifest_dir_btree_is_leaf(hdr: *const u8, body: *const u8, body_len: usize) -> c_int {
    let Some(m) = (unsafe { manifest(hdr, body, body_len) }) else { return -1 };
    match m.dir_btree_is_leaf() {
        Some(true) => 1,
        Some(false) => 0,
        None => -1,
    }
}

macro_rules! manifest_dirent {
    ($name:ident, $method:ident) => {
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $name(
            hdr: *const u8, body: *const u8, body_len: usize, idx: u32,
            out_inode: *mut u64, out_name: *mut *const c_char, out_name_len: *mut u16,
        ) -> c_int {
            let Some(m) = (unsafe { manifest(hdr, body, body_len) }) else { return EINVAL };
            match m.$method(idx) {
                Ok(e) => {
                    unsafe {
                        put(out_inode, e.val);
                        put(out_name, e.name.as_ptr().cast::<c_char>());
                        put(out_name_len, e.name.len() as u16);
                    }
                    OK
                }
                Err(e) => e,
            }
        }
    };
}

manifest_dirent!(tessera_rs_manifest_dirent_at, dirent_at);
manifest_dirent!(tessera_rs_manifest_dir_btree_leaf_at, dir_btree_leaf_at);

/// Whole-node check of a DIRECTORY_BTREE manifest blob (see
/// `manifest::dir_btree_node`). On success: leaf flag, the entries after the
/// 8-byte node header, and an entry count proven to fit.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessera_rs_dir_btree_node(
    blob: *const u8, len: usize, out_leaf: *mut c_int,
    out_entries: *mut *const u8, out_len: *mut usize, out_count: *mut u32,
) -> c_int {
    let Some(b) = (unsafe { bytes(blob, len) }) else { return EINVAL };
    match crate::manifest::dir_btree_node(b) {
        Ok(n) => {
            unsafe {
                put(out_leaf, n.leaf as c_int);
                put(out_entries, n.entries.as_ptr());
                put(out_len, n.entries.len());
                put(out_count, n.count);
            }
            OK
        }
        Err(e) => e,
    }
}

// ---- pack.h ---------------------------------------------------------------

/// Validates a whole pack; fills `out_geom` and copies the 4 KiB header to
/// `out_hdr`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessera_rs_pack_open(data: *const u8, len: usize, out_geom: *mut Geometry, out_hdr: *mut u8) -> c_int {
    let Some(d) = (unsafe { bytes(data, len) }) else { return EINVAL };
    if out_geom.is_null() || out_hdr.is_null() {
        return EINVAL;
    }
    match pack::open(d) {
        Ok(g) => match d.get(..SECTOR).and_then(|h| <&[u8; SECTOR]>::try_from(h).ok()) {
            Some(h) => {
                unsafe {
                    put(out_geom, g);
                    copy_out(h, out_hdr);
                }
                OK
            }
            None => EINVAL,
        },
        Err(e) => e,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessera_rs_pack_bloom_might_contain(
    data: *const u8, len: usize, geom: *const Geometry, hash: *const u8,
) -> c_int {
    let (Some(d), Some(g), Some(h)) =
        (unsafe { bytes(data, len) }, unsafe { geom.as_ref() }, unsafe { arr::<HASH_SIZE>(hash) })
    else {
        return 1;
    };
    pack::bloom_might_contain(d, g, h) as c_int
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessera_rs_pack_lookup(
    data: *const u8, len: usize, geom: *const Geometry, hash: *const u8,
    out_bytes: *mut *const u8, out_len: *mut u32,
) -> c_int {
    let (Some(d), Some(g), Some(h)) =
        (unsafe { bytes(data, len) }, unsafe { geom.as_ref() }, unsafe { arr::<HASH_SIZE>(hash) })
    else {
        return EINVAL;
    };
    if out_bytes.is_null() || out_len.is_null() {
        return EINVAL;
    }
    match pack::lookup(d, g, h) {
        Ok((at, n)) => match d.get(at..) {
            Some(rest) => {
                unsafe {
                    put(out_bytes, rest.as_ptr());
                    put(out_len, n);
                }
                OK
            }
            None => crate::error::ECORRUPT,
        },
        Err(e) => e,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessera_rs_pack_blob_hash_at(
    data: *const u8, len: usize, geom: *const Geometry, index: u32, out: *mut u8,
) -> c_int {
    let (Some(d), Some(g)) = (unsafe { bytes(data, len) }, unsafe { geom.as_ref() }) else { return EINVAL };
    if out.is_null() {
        return EINVAL;
    }
    match pack::blob_hash_at(d, g, index).map(<&[u8; HASH_SIZE]>::try_from) {
        Ok(Ok(h)) => {
            unsafe { copy_out(h, out) };
            OK
        }
        Ok(Err(_)) => crate::error::ECORRUPT,
        Err(e) => e,
    }
}

// ---- btree.c ----------------------------------------------------------------

/// `load_node`'s checks on a node already read into `block`. Returns OK or
/// ECORRUPT; on failure `*out_fail` is a `tessera_btree_fail_t` and, for a
/// node of another tree, `*out_found_kind` its tree kind. `out_hdr` (64
/// bytes, may be NULL) receives the header whenever its magic and CRC are good,
/// for the caller's diagnostics.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessera_rs_btree_node_check(
    block: *const u8, block_len: usize, tree_kind: u8, key_size: u32, value_size: u32,
    leaf_fanout: u32, inner_fanout: u32,
    out_hdr: *mut u8, out_fail: *mut c_int, out_found_kind: *mut u8,
) -> c_int {
    let Some(b) = (unsafe { bytes(block, block_len) }) else { return EINVAL };
    if !out_hdr.is_null() {
        if let Some(h) = b.get(..BTREE_NODE_HEADER_SIZE).and_then(|h| <&[u8; BTREE_NODE_HEADER_SIZE]>::try_from(h).ok()) {
            if codec::btree_node_header(h).is_ok() {
                unsafe { copy_out(h, out_hdr) };
            }
        }
    }
    let shape = Shape { tree_kind, key_size, value_size, leaf_fanout, inner_fanout };
    match btree::node_check(b, &shape) {
        Ok(()) => {
            unsafe { put(out_fail, btree::FAIL_NONE) };
            OK
        }
        Err(f) => {
            unsafe {
                put(out_fail, f.fail);
                put(out_found_kind, f.found_kind);
            }
            crate::error::ECORRUPT
        }
    }
}

/// `search_leaf`. `key` is `key_size` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessera_rs_btree_search(
    block: *const u8, block_len: usize, entry_count: u32, key_size: u32, entry_size: u32,
    key: *const u8, out_exact: *mut c_int,
) -> c_int {
    let (Some(b), Some(k)) = (unsafe { bytes(block, block_len) }, unsafe { bytes(key, key_size as usize) }) else {
        unsafe { put(out_exact, 0) };
        return -1;
    };
    let (idx, exact) = btree::search(b, entry_count, key_size, entry_size, k);
    unsafe { put(out_exact, exact as c_int) };
    idx
}

// ---- journal.c --------------------------------------------------------------

/// `read_record`'s header checks on the record's first block. The header is
/// copied to `out_hdr` (64 bytes) once its magic and CRC are good.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessera_rs_journal_record_check(blk: *const u8, cap: u64, out_hdr: *mut u8) -> c_int {
    let Some(b) = (unsafe { arr::<SECTOR>(blk) }) else { return EINVAL };
    if out_hdr.is_null() {
        return EINVAL;
    }
    if let Some(h) = b.get(..RECORD_HEADER_SIZE).and_then(|h| <&[u8; RECORD_HEADER_SIZE]>::try_from(h).ok()) {
        if codec::record_header(h).is_ok() {
            unsafe { copy_out(h, out_hdr) };
        }
    }
    rc(journal::record_check(b, cap))
}

#[unsafe(no_mangle)]
pub extern "C" fn tessera_rs_journal_advance(block: u64, block_count: u32, cap: u64) -> u64 {
    journal::advance(block, block_count, cap)
}
