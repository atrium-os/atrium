//! Shared internals for the Tessera CLI tools.
//!
//! The block-I/O glue lives here so both `mkfs-tessera` and
//! `tessera-debug` link against the same implementation. It uses
//! `pread`/`pwrite` against a raw fd — no caching layer; the kernel
//! page cache is sufficient for the tooling workloads.

use std::ffi::c_void;
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
#[allow(unused_imports)]
use std::os::unix::fs::FileExt;	/* read_at, for the blob-index scan */

use tessera_sys::*;

pub const SECTOR_SIZE: u64 = 4096;

pub struct DiskCtx {
    pub fd: i32,
    /// Next free sector for the metadata-reserve bump allocator (repair).
    /// `bump_max == 0` disables allocation — read-only tools keep the
    /// original stub behaviour (alloc returns -1).
    pub bump: std::cell::Cell<u64>,
    /// One past the last allocatable reserve sector (meta_reserve_start +
    /// meta_reserve_length). 0 = allocation disabled.
    pub bump_max: u64,
}

impl DiskCtx {
    /// Read-only context: allocation disabled.
    pub fn ro(fd: i32) -> Self {
        DiskCtx { fd, bump: std::cell::Cell::new(0), bump_max: 0 }
    }
    /// Read-write context with a metadata-reserve bump allocator over
    /// [bump_start, bump_max). Used by tessera-fsck --repair to allocate
    /// COW btree / free-tree nodes exactly like the kmod's runtime bump.
    pub fn rw(fd: i32, bump_start: u64, bump_max: u64) -> Self {
        DiskCtx { fd, bump: std::cell::Cell::new(bump_start), bump_max }
    }
}

pub extern "C" fn disk_read(ctx: *mut c_void, sector: u64, out: *mut u8) -> i32 {
    let d = unsafe { &*(ctx as *const DiskCtx) };
    let n = unsafe {
        libc::pread(
            d.fd,
            out as *mut c_void,
            SECTOR_SIZE as usize,
            (sector * SECTOR_SIZE) as i64,
        )
    };
    if n == SECTOR_SIZE as isize { 0 } else { -1 }
}

pub extern "C" fn disk_write(ctx: *mut c_void, sector: u64, buf: *const u8) -> i32 {
    let d = unsafe { &*(ctx as *const DiskCtx) };
    let n = unsafe {
        libc::pwrite(
            d.fd,
            buf as *const c_void,
            SECTOR_SIZE as usize,
            (sector * SECTOR_SIZE) as i64,
        )
    };
    if n == SECTOR_SIZE as isize { 0 } else { -1 }
}

/// Metadata-reserve bump allocator. Mirrors core's `fmt_alloc`: hand out
/// `n` contiguous sectors from the reserve, advancing the bump pointer.
/// Read-only contexts (bump_max == 0) return -1, preserving the old stub
/// behaviour for tools that never mutate. On exhaustion returns -1
/// (ENOSPC) — the repair aborts before committing a partial change.
pub extern "C" fn disk_alloc(ctx: *mut c_void, n: u64, out_start: *mut u64) -> i32 {
    let d = unsafe { &*(ctx as *const DiskCtx) };
    if d.bump_max == 0 {
        return -1;
    }
    let start = d.bump.get();
    if start + n > d.bump_max {
        return -1;
    }
    d.bump.set(start + n);
    unsafe { *out_start = start; }
    0
}
/// Free is a no-op bump-back is not tracked: retired COW nodes leak into
/// the reserve until the next `tessera repack`, exactly as the runtime
/// bump allocator behaves. Offline repair touches only a handful of
/// nodes, so the leak is negligible.
pub extern "C" fn disk_free(_: *mut c_void, _: u64, _: u64) -> i32 { 0 }

pub fn make_io(ctx: &mut DiskCtx) -> tessera_block_io_t {
    tessera_block_io_t {
        read_block:  Some(disk_read),
        write_block: Some(disk_write),
        alloc:       Some(disk_alloc),
        free:        Some(disk_free),
        ctx:         ctx as *mut DiskCtx as *mut c_void,
    }
}

/// Read 16 bytes from /dev/urandom for a v4-shape UUID. Sets the
/// version + variant bits per RFC 4122 §4.4 so external consumers can
/// recognise it as a "random" UUID, but Tessera doesn't otherwise
/// interpret the bytes.
pub fn random_uuid_v4() -> io::Result<[u8; 16]> {
    use std::io::Read;
    let mut buf = [0u8; 16];
    let mut f = File::open("/dev/urandom")?;
    f.read_exact(&mut buf)?;
    buf[6] = (buf[6] & 0x0f) | 0x40;     /* version 4 */
    buf[8] = (buf[8] & 0x3f) | 0x80;     /* variant 1 */
    Ok(buf)
}

pub fn format_uuid(u: &[u8; 16]) -> String {
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-\
         {:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        u[0], u[1], u[2], u[3], u[4], u[5], u[6], u[7],
        u[8], u[9], u[10], u[11], u[12], u[13], u[14], u[15]
    )
}

pub fn open_file_rw(path: &str) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_SYNC)
        .open(path)
}

pub fn open_file_ro(path: &str) -> io::Result<File> {
    std::fs::OpenOptions::new().read(true).open(path)
}

pub fn fd_of(f: &File) -> i32 {
    f.as_raw_fd()
}

pub fn file_size(f: &File) -> io::Result<u64> {
    let m = f.metadata()?;
    let len = m.len();
    if len > 0 {
        return Ok(len);
    }
    // Block / character device: stat reports st_size==0. Use the
    // FreeBSD DIOCGMEDIASIZE ioctl to get the actual extent.
    #[cfg(target_os = "freebsd")]
    {
        use std::os::unix::io::AsRawFd;
        // _IOR('d', 129, off_t) — see <sys/disk.h>.
        const DIOCGMEDIASIZE: libc::c_ulong = 0x40086481;
        let mut sz: i64 = 0;
        let r = unsafe {
            libc::ioctl(f.as_raw_fd(), DIOCGMEDIASIZE, &mut sz as *mut i64)
        };
        if r == 0 && sz > 0 {
            return Ok(sz as u64);
        }
    }
    Ok(len)
}

/* ── blob→pack index rebuild (shared by tessera-reindex and tessera-repack) ──
 *
 * Lives here rather than in reindex's binary because tessera-repack must do
 * the same work: repack rewrites the meta-reserve, so the old index would
 * point at reused sectors and MUST be dropped — and the window between
 * "repack finished" and "operator remembers to run tessera-reindex" is
 * exactly when a reboot turns into a multi-minute apparent hang (#75).
 *
 * Scan + build only; the caller commits. That keeps the crash story simple:
 * the tree is built in fresh reserve sectors nothing references yet, so an
 * interrupted rebuild leaves blob_index_root untouched — i.e. absent, which
 * is the same state repack already produces and which tessera-debug and
 * tessera-fsck now report explicitly.
 */
const TT_SECTOR: u64 = 4096;
const TT_FLAG_MULTI_EXTENT: u32 = 1 << 2;
const TT_PEL_MAGIC: u64 = 0x315056454C455054; // "TPELEV01"

/// Walk a multi-extent pack's PEL chain → its data extents (start, len sectors).
fn tt_resolve_pel(f: &std::fs::File, head: u64) -> Option<Vec<(u64, u64)>> {
    let mut exts = Vec::new();
    let mut cur = head;
    let mut guard = 0;
    while cur != 0 && guard < 512 {
        let mut b = [0u8; 4096];
        if f.read_at(&mut b, cur * TT_SECTOR).ok()? != 4096 { return None; }
        if u64::from_le_bytes(b[0..8].try_into().ok()?) != TT_PEL_MAGIC { return None; }
        let ec = u32::from_le_bytes(b[12..16].try_into().ok()?) as usize;
        let next = u64::from_le_bytes(b[24..32].try_into().ok()?);
        for i in 0..ec {
            let o = 32 + i * 16;
            if o + 16 > 4096 { return None; }
            let s = u64::from_le_bytes(b[o..o + 8].try_into().ok()?);
            let l = u64::from_le_bytes(b[o + 8..o + 16].try_into().ok()?);
            exts.push((s, l));
        }
        cur = next; guard += 1;
    }
    if exts.is_empty() { None } else { Some(exts) }
}

/// Read a multi-extent pack's whole body into RAM (rare: PEL data extents
/// concatenated). Used only for the multi-extent slow path.
fn tt_read_pack_body_multi(f: &std::fs::File, start: u64) -> Option<Vec<u8>> {
    let exts = tt_resolve_pel(f, start)?;
    let mut buf = Vec::new();
    for (s, l) in exts {
        let mut e = vec![0u8; (l * TT_SECTOR) as usize];
        if f.read_at(&mut e, s * TT_SECTOR).ok()? != e.len() { return None; }
        buf.extend_from_slice(&e);
    }
    Some(buf)
}

/// Collect a pack's blob hashes (32 bytes each) into `out`. Single-extent packs
/// (the overwhelming common case) read only the 1-sector header + index_blocks
/// index sectors — ~2 sectors, not the whole pack body. Multi-extent packs fall
/// back to reading the full body + tessera_pack_open.
fn tt_collect_pack_hashes(f: &std::fs::File, start: u64, flags: u32, out: &mut Vec<([u8; 32], [u8; 16])>, pack_id: [u8; 16]) {
    if flags & TT_FLAG_MULTI_EXTENT != 0 {
        if let Some(body) = tt_read_pack_body_multi(f, start) {
            unsafe {
                let pr = tessera_pack_open(body.as_ptr(), body.len());
                if !pr.is_null() {
                    for j in 0..tessera_pack_blob_count(pr) {
                        let mut h = [0u8; 32];
                        if tessera_pack_blob_hash_at(pr, j, h.as_mut_ptr()) == 0 {
                            out.push((h, pack_id));
                        }
                    }
                    tessera_pack_close(pr);
                }
            }
        }
        return;
    }
    // Single-extent fast path: header sector (@start) → blob_count/index_blocks,
    // then the index sectors (@start+1). Header layout: blob_count@48,
    // index_blocks@52; index entries are 48 bytes with the hash first.
    let mut hdr = [0u8; 4096];
    if f.read_at(&mut hdr, start * TT_SECTOR).map(|n| n == 4096).unwrap_or(false) == false { return; }
    if &hdr[0..5] != b"TPACK" { return; }
    let blob_count = u32::from_le_bytes(hdr[48..52].try_into().unwrap()) as usize;
    let index_blocks = u32::from_le_bytes(hdr[52..56].try_into().unwrap()) as usize;
    if index_blocks == 0 || blob_count == 0 { return; }
    let mut idx = vec![0u8; index_blocks * 4096];
    if f.read_at(&mut idx, (start + 1) * TT_SECTOR).map(|n| n == idx.len()).unwrap_or(false) == false { return; }
    for j in 0..blob_count {
        let o = j * 48;
        if o + 32 > idx.len() { break; }
        let mut h = [0u8; 32];
        h.copy_from_slice(&idx[o..o + 32]);
        out.push((h, pack_id));
    }
}

/// Scan every pack and build a fresh blob→pack index tree in the reserve.
/// Returns (root, distinct blobs, packs scanned, new bump). Does NOT commit.
pub fn rebuild_blob_index(
    f: &File,
    io: &tessera_block_io_t,
    ctxp: *mut DiskCtx,
    pack_root: u64,
) -> Result<(u64, usize, u64, u64), String> {
    if pack_root == 0 { return Err("volume has no pack registry".into()); }
    let mut pairs: Vec<([u8; 32], [u8; 16])> = Vec::new();
    let mut npacks = 0u64;
    unsafe {
        let t = tessera_btree_open(io, pack_root, TESSERA_BTREE_KIND_PACK_REG, 16,
            TESSERA_REGISTRY_ENTRY_SIZE);
        if t.is_null() { return Err("open pack registry".into()); }
        let c = tessera_btree_seek_first(t);
        if !c.is_null() {
            let mut key = [0u8; 16];
            let mut val = vec![0u8; TESSERA_REGISTRY_ENTRY_SIZE as usize];
            loop {
                if tessera_btree_cursor_get(c, key.as_mut_ptr(), val.as_mut_ptr()) != 0 { break; }
                npacks += 1;
                let start = u64::from_le_bytes(val[16..24].try_into().unwrap());
                let flags = u32::from_le_bytes(val[60..64].try_into().unwrap());
                tt_collect_pack_hashes(f, start, flags, &mut pairs, key);
                if tessera_btree_cursor_next(c) != 0 { break; }
            }
            tessera_btree_cursor_free(c);
        }
        tessera_btree_close(t);
    }
    // A blob may live in more than one pack; any one resolves it.
    pairs.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    pairs.dedup_by(|a, b| a.0 == b.0);

    let mut keys = Vec::with_capacity(pairs.len() * 32);
    let mut vals = Vec::with_capacity(pairs.len() * 16);
    for (h, p) in &pairs { keys.extend_from_slice(h); vals.extend_from_slice(p); }

    let mut root: u64 = 0;
    unsafe {
        let t = tessera_btree_create(io, TESSERA_BTREE_KIND_BLOB_INDEX, 32, 16, &mut root);
        if t.is_null() { return Err("create blob-index tree (reserve full?)".into()); }
        if !pairs.is_empty() {
            let mut nr = root;
            let rc = tessera_btree_put_sorted_batch(t, keys.as_ptr(), vals.as_ptr(),
                pairs.len() as u32, &mut nr);
            if rc != 0 {
                tessera_btree_close(t);
                return Err(format!("build index rc={rc} (reserve full?)"));
            }
            root = nr;
        }
        tessera_btree_close(t);
    }
    let new_bump = unsafe { (*ctxp).bump.get() };
    Ok((root, pairs.len(), npacks, new_bump))
}
