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
use tessera_tools::{rebuild_blob_index, fd_of, make_io, open_file_rw, DiskCtx};


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

    // Shared with tessera-repack, which must rebuild the same index after it
    // rewrites the reserve (#75).
    let (root, nblobs, npacks, new_bump) = rebuild_blob_index(&f, &io, ctxp, pack_root)?;
    println!("  scanned {npacks} packs, {nblobs} distinct blobs to index");
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
        // Ignored: TESSERA_COMMIT_DEAD_EXTENT is not set, so the
        // superblock's own dead_extent_root is preserved.
        dead_extent_root:   0,
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
