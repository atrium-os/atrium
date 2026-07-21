//! tessera-reindex — build (or rebuild) the on-disk blob→pack index.
//!
//! The index (TESSERA_BTREE_KIND_BLOB_INDEX: 32-byte blob hash → 16-byte
//! pack_id) lets the loader and kmod resolve a content-hash to its pack in
//! O(log n) instead of scanning the whole pack registry — the difference
//! between a ~5-minute and a ~1-second cold read of a multi-pack file on a large
//! root. This tool populates it for an existing volume (mkfs leaves it empty;
//! new writes maintain it; repack drops it). Volume must be UNMOUNTED.
//!
//! One pass over the pack registry collects every (blob hash → pack_id) pair,
//! sorts them, and bulk-builds the index tree in the meta-reserve, then commits
//! the new root. Crash-safe: the tree is written to fresh (unreferenced) reserve
//! sectors above the current bump and sealed with one atomic dual-SB commit — a
//! crash leaves the old superblock intact.

use std::os::unix::fs::FileExt;
use std::process::ExitCode;
use tessera_sys::*;
use tessera_tools::{fd_of, make_io, open_file_rw, DiskCtx};

const SECTOR: u64 = 4096;
const REGENT: u32 = TESSERA_REGISTRY_ENTRY_SIZE;
const FLAG_MULTI_EXTENT: u32 = 1 << 2;
const PEL_MAGIC: u64 = 0x315056454C455054; // "TPELEV01"

/// Walk a multi-extent pack's PEL chain → its data extents (start, len sectors).
fn resolve_pel(f: &std::fs::File, head: u64) -> Option<Vec<(u64, u64)>> {
    let mut exts = Vec::new();
    let mut cur = head;
    let mut guard = 0;
    while cur != 0 && guard < 512 {
        let mut b = [0u8; 4096];
        if f.read_at(&mut b, cur * SECTOR).ok()? != 4096 { return None; }
        if u64::from_le_bytes(b[0..8].try_into().ok()?) != PEL_MAGIC { return None; }
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
fn read_pack_body_multi(f: &std::fs::File, start: u64) -> Option<Vec<u8>> {
    let exts = resolve_pel(f, start)?;
    let mut buf = Vec::new();
    for (s, l) in exts {
        let mut e = vec![0u8; (l * SECTOR) as usize];
        if f.read_at(&mut e, s * SECTOR).ok()? != e.len() { return None; }
        buf.extend_from_slice(&e);
    }
    Some(buf)
}

/// Collect a pack's blob hashes (32 bytes each) into `out`. Single-extent packs
/// (the overwhelming common case) read only the 1-sector header + index_blocks
/// index sectors — ~2 sectors, not the whole pack body. Multi-extent packs fall
/// back to reading the full body + tessera_pack_open.
fn collect_pack_hashes(f: &std::fs::File, start: u64, flags: u32, out: &mut Vec<([u8; 32], [u8; 16])>, pack_id: [u8; 16]) {
    if flags & FLAG_MULTI_EXTENT != 0 {
        if let Some(body) = read_pack_body_multi(f, start) {
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
    if f.read_at(&mut hdr, start * SECTOR).map(|n| n == 4096).unwrap_or(false) == false { return; }
    if &hdr[0..5] != b"TPACK" { return; }
    let blob_count = u32::from_le_bytes(hdr[48..52].try_into().unwrap()) as usize;
    let index_blocks = u32::from_le_bytes(hdr[52..56].try_into().unwrap()) as usize;
    if index_blocks == 0 || blob_count == 0 { return; }
    let mut idx = vec![0u8; index_blocks * 4096];
    if f.read_at(&mut idx, (start + 1) * SECTOR).map(|n| n == idx.len()).unwrap_or(false) == false { return; }
    for j in 0..blob_count {
        let o = j * 48;
        if o + 32 > idx.len() { break; }
        let mut h = [0u8; 32];
        h.copy_from_slice(&idx[o..o + 32]);
        out.push((h, pack_id));
    }
}

fn run(path: &str) -> Result<i32, String> {
    let f = open_file_rw(path).map_err(|e| format!("open {path} (rw): {e}"))?;
    let mut ctx = DiskCtx::rw(fd_of(&f), 0, 0);
    let io = make_io(&mut ctx);
    // Access ctx only through io.ctx's provenance across FFI (aliasing rule).
    let ctxp = io.ctx as *mut DiskCtx;

    let mut v: *mut tessera_volume_t = std::ptr::null_mut();
    if unsafe { tessera_volume_open(&io, &mut v) } != 0 {
        return Err("SUPERBLOCK INVALID (tessera_volume_open failed)".into());
    }
    let mr_start  = unsafe { tessera_volume_meta_reserve_start(v) };
    let mr_len    = unsafe { tessera_volume_meta_reserve_length(v) };
    let mr_bump   = unsafe { tessera_volume_meta_reserve_bump(v) };
    let pack_root = unsafe { tessera_volume_pack_registry_root(v) };
    unsafe { (*ctxp).bump.set(mr_bump); (*ctxp).bump_max = mr_start + mr_len; }

    println!("tessera-reindex: {path}");
    if pack_root == 0 { return Err("volume has no pack registry".into()); }

    // One pass over the registry: collect every (blob hash → pack_id).
    let mut pairs: Vec<([u8; 32], [u8; 16])> = Vec::new();
    let mut npacks = 0u64;
    unsafe {
        let t = tessera_btree_open(&io, pack_root, TESSERA_BTREE_KIND_PACK_REG, 16, REGENT);
        if t.is_null() { return Err("open pack registry".into()); }
        let c = tessera_btree_seek_first(t);
        if !c.is_null() {
            let mut key = [0u8; 16];
            let mut val = vec![0u8; REGENT as usize];
            loop {
                if tessera_btree_cursor_get(c, key.as_mut_ptr(), val.as_mut_ptr()) != 0 { break; }
                npacks += 1;
                let start = u64::from_le_bytes(val[16..24].try_into().unwrap());
                let flags = u32::from_le_bytes(val[60..64].try_into().unwrap());
                collect_pack_hashes(&f, start, flags, &mut pairs, key);
                if tessera_btree_cursor_next(c) != 0 { break; }
            }
            tessera_btree_cursor_free(c);
        }
        tessera_btree_close(t);
    }
    println!("  scanned {npacks} packs, {} blob refs", pairs.len());

    // Sort by hash + dedup (a blob may be stored in >1 pack; any is fine).
    pairs.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    pairs.dedup_by(|a, b| a.0 == b.0);
    println!("  {} distinct blobs to index", pairs.len());

    // Bulk-build the index tree from the sorted pairs (minimal node churn).
    let mut keys = Vec::with_capacity(pairs.len() * 32);
    let mut vals = Vec::with_capacity(pairs.len() * 16);
    for (h, p) in &pairs { keys.extend_from_slice(h); vals.extend_from_slice(p); }

    let mut root: u64 = 0;
    unsafe {
        let t = tessera_btree_create(&io, TESSERA_BTREE_KIND_BLOB_INDEX, 32, 16, &mut root);
        if t.is_null() { return Err("create blob-index tree (reserve full?)".into()); }
        if !pairs.is_empty() {
            let mut nr = root;
            let rc = tessera_btree_put_sorted_batch(t, keys.as_ptr(), vals.as_ptr(),
                pairs.len() as u32, &mut nr);
            if rc != 0 { tessera_btree_close(t); return Err(format!("build index rc={rc} (reserve full?)")); }
            root = nr;
        }
        tessera_btree_close(t);
    }
    let new_bump = unsafe { (*ctxp).bump.get() };

    // Commit: set blob_index_root, preserve every other root; bump grew (we
    // appended to the reserve) so the default grow-only commit is correct.
    let commit = tessera_commit_roots_t {
        inode_root:         unsafe { tessera_volume_inode_root(v) },
        pack_registry_root: pack_root,
        free_extent_root:   unsafe { tessera_volume_free_extent_root(v) },
        quota_tree_root:    unsafe { tessera_volume_quota_tree_root(v) },
        snapshots_root:     unsafe { tessera_volume_snapshots_root(v) },
        meta_reserve_bump:  new_bump,
        next_inode_no:      unsafe { tessera_volume_next_inode_no(v) },
        blob_index_root:    root,
    };
    let rc = unsafe { tessera_volume_commit_roots(v, &commit) };
    unsafe { tessera_volume_close(v) };
    if rc != 0 { return Err(format!("commit_roots rc={rc}")); }

    println!("  index committed: root@{root}, {} reserve sectors used",
        new_bump.saturating_sub(mr_bump));
    println!("tessera-reindex: DONE — cold reads now resolve blob→pack in O(log n).");
    Ok(0)
}

fn main() -> ExitCode {
    let args: Vec<_> = std::env::args().collect();
    let path = args.iter().skip(1).find(|a| !a.starts_with('-'));
    let path = match path {
        Some(p) => p.clone(),
        None => {
            eprintln!("usage: tessera-reindex PATH");
            eprintln!("  build the on-disk blob→pack index (volume must be UNMOUNTED)");
            return ExitCode::from(2);
        }
    };
    match run(&path) {
        Ok(code) => ExitCode::from(code as u8),
        Err(e) => { eprintln!("tessera-reindex: {e}"); ExitCode::from(1) }
    }
}
