//! tessera-repack — offline meta-reserve recovery.
//!
//! The metadata-reserve bump pointer only ever advances: under sustained
//! commit churn (esp. with many retained snapshots) it can climb to the reserve
//! ceiling and stay there, leaving the volume permanently dependent on the
//! recycle free-list with no bump headroom. This tool recovers that state
//! OFFLINE (volume must be UNMOUNTED): it retires all retained snapshots, then
//! rewrites the four live metadata trees (inode / pack-registry / free-extent /
//! quota) compactly from the reserve start and lowers the bump to the compacted
//! frontier. Data (packs / file content) is untouched — only metadata b-tree
//! nodes move, and only snapshot *history* is dropped.
//!
//! NOT crash-safe: compacting a maxed reserve overwrites old metadata in place
//! (there is no pristine room to stage into), so a crash mid-repack leaves the
//! old superblock pointing at half-overwritten trees. `commit_roots` seals the
//! new state atomically at the end, but BACK UP THE VOLUME FIRST. Gated behind
//! -y/--apply; without it, this only reports the current reserve state.

use std::process::ExitCode;
use tessera_sys::*;
use tessera_tools::{fd_of, make_io, open_file_ro, open_file_rw, DiskCtx};

/// Read every (key, value) of a b-tree into RAM. Empty vec if root == 0.
fn read_tree(io: &tessera_block_io_t, root: u64, kind: u8, ksz: u32, vsz: u32)
    -> Result<Vec<(Vec<u8>, Vec<u8>)>, String>
{
    if root == 0 { return Ok(Vec::new()); }
    let mut out = Vec::new();
    unsafe {
        let t = tessera_btree_open(io, root, kind, ksz, vsz);
        if t.is_null() { return Err(format!("open tree kind={kind} root={root}")); }
        let c = tessera_btree_seek_first(t);
        if !c.is_null() {
            loop {
                let mut key = vec![0u8; ksz as usize];
                let mut val = vec![0u8; vsz as usize];
                if tessera_btree_cursor_get(c, key.as_mut_ptr(), val.as_mut_ptr()) != 0 {
                    break;
                }
                out.push((key, val));
                if tessera_btree_cursor_next(c) != 0 { break; }
            }
            tessera_btree_cursor_free(c);
        }
        tessera_btree_close(t);
    }
    Ok(out)
}

/// Create a fresh b-tree (allocating nodes from ctx.bump) and insert all
/// entries. Returns the compacted root. If `existed` is false (old root was 0)
/// the tree is left absent (root 0).
fn write_tree(io: &tessera_block_io_t, existed: bool, kind: u8, ksz: u32, vsz: u32,
    entries: &[(Vec<u8>, Vec<u8>)]) -> Result<u64, String>
{
    if !existed { return Ok(0); }
    unsafe {
        let mut root: u64 = 0;
        let t = tessera_btree_create(io, kind, ksz, vsz, &mut root);
        if t.is_null() { return Err(format!("create tree kind={kind}")); }
        for (k, v) in entries {
            let mut nr = root;
            let rc = tessera_btree_put(t, k.as_ptr(), v.as_ptr(), &mut nr);
            if rc != 0 { tessera_btree_close(t); return Err(format!("put kind={kind} rc={rc}")); }
            root = nr;
        }
        tessera_btree_close(t);
        Ok(root)
    }
}

fn run(path: &str, apply: bool) -> Result<i32, String> {
    let f = if apply {
        open_file_rw(path).map_err(|e| format!("open {path} (rw): {e}"))?
    } else {
        open_file_ro(path).map_err(|e| format!("open {path}: {e}"))?
    };
    let mut ctx = DiskCtx::ro(fd_of(&f));
    let io = make_io(&mut ctx);

    let mut v: *mut tessera_volume_t = std::ptr::null_mut();
    if unsafe { tessera_volume_open(&io, &mut v) } != 0 {
        return Err("SUPERBLOCK INVALID (tessera_volume_open failed)".into());
    }

    let inode_root  = unsafe { tessera_volume_inode_root(v) };
    let pack_root   = unsafe { tessera_volume_pack_registry_root(v) };
    let free_root   = unsafe { tessera_volume_free_extent_root(v) };
    let quota_root  = unsafe { tessera_volume_quota_tree_root(v) };
    let snap_root   = unsafe { tessera_volume_snapshots_root(v) };
    let next_ino    = unsafe { tessera_volume_next_inode_no(v) };
    let generation  = unsafe { tessera_volume_generation(v) };
    let mr_start    = unsafe { tessera_volume_meta_reserve_start(v) };
    let mr_len      = unsafe { tessera_volume_meta_reserve_length(v) };
    let mr_bump     = unsafe { tessera_volume_meta_reserve_bump(v) };
    let used = mr_bump - mr_start;

    println!("tessera-repack: {path}");
    println!("  generation:    {generation}");
    println!("  meta reserve:  bump {used} / {mr_len} sectors ({:.1}% of ceiling)",
        100.0 * used as f64 / mr_len as f64);
    println!("  snapshots:     {}", if snap_root == 0 { "none".into() } else { format!("root@{snap_root} (will be RETIRED)") });

    if !apply {
        let pct = 100.0 * used as f64 / mr_len as f64;
        if pct < 50.0 {
            println!("  reserve is healthy ({pct:.1}% of ceiling) — repack likely UNNECESSARY.");
            println!("  (repack helps only when the bump is stuck high; on a healthy volume it");
            println!("   drops snapshots for no reserve gain.)");
        } else {
            println!("  reserve is HIGH ({pct:.1}% of ceiling) — repack recommended.");
        }
        println!("  re-run with -y/--apply to compact — retires snapshots, rewrites the 4 live");
        println!("  trees from the reserve start, lowers the bump. BACK UP FIRST: an in-place");
        println!("  compaction is not crash-safe.");
        unsafe { tessera_volume_close(v) };
        return Ok(0);
    }

    // Read every entry of the four live trees into RAM *before* touching the
    // reserve (the rewrite below overwrites the sectors we are reading).
    let inodes = read_tree(&io, inode_root, TESSERA_BTREE_KIND_INODE, 4, TESSERA_INODE_RECORD_SIZE)?;
    let packs  = read_tree(&io, pack_root,  TESSERA_BTREE_KIND_PACK_REG, 16, TESSERA_REGISTRY_ENTRY_SIZE)?;
    let frees  = read_tree(&io, free_root,  TESSERA_BTREE_KIND_FREE_EXT, 8, 8)?;
    let quotas = read_tree(&io, quota_root, TESSERA_BTREE_KIND_QUOTA, 8, 128)?;
    println!("  read: {} inodes, {} packs, {} free-extents, {} quota domains",
        inodes.len(), packs.len(), frees.len(), quotas.len());

    // Compact from the reserve start.
    ctx.bump.set(mr_start);
    ctx.bump_max = mr_start + mr_len;

    let new_inode = write_tree(&io, inode_root != 0, TESSERA_BTREE_KIND_INODE, 4, TESSERA_INODE_RECORD_SIZE, &inodes)?;
    let new_pack  = write_tree(&io, pack_root  != 0, TESSERA_BTREE_KIND_PACK_REG, 16, TESSERA_REGISTRY_ENTRY_SIZE, &packs)?;
    let new_free  = write_tree(&io, free_root  != 0, TESSERA_BTREE_KIND_FREE_EXT, 8, 8, &frees)?;
    let new_quota = write_tree(&io, quota_root != 0, TESSERA_BTREE_KIND_QUOTA, 8, 128, &quotas)?;

    let new_bump = ctx.bump.get();
    let commit = tessera_commit_roots_t {
        inode_root:         new_inode,
        pack_registry_root: new_pack,
        free_extent_root:   new_free,
        quota_tree_root:    new_quota,
        snapshots_root:     0,          // retire all retained snapshots
        meta_reserve_bump:  new_bump,
        next_inode_no:      next_ino,
    };
    let rc = unsafe { tessera_volume_commit_roots(v, &commit) };
    unsafe { tessera_volume_close(v) };
    if rc != 0 { return Err(format!("commit_roots failed: rc={rc}")); }

    let new_used = new_bump - mr_start;
    println!("  compacted:     bump {used} -> {new_used} sectors  (reclaimed {} sectors headroom)",
        used.saturating_sub(new_used));
    println!("tessera-repack: DONE — run tessera-fsck to verify.");
    Ok(0)
}

fn main() -> ExitCode {
    let args: Vec<_> = std::env::args().collect();
    let mut path = None;
    let mut apply = false;
    for a in &args[1..] {
        match a.as_str() {
            "-y" | "--apply" => apply = true,
            s if !s.starts_with('-') => path = Some(s.to_string()),
            _ => {}
        }
    }
    let path = match path {
        Some(p) => p,
        None => {
            eprintln!("usage: tessera-repack [-y|--apply] PATH");
            eprintln!("  offline meta-reserve compaction (volume must be UNMOUNTED)");
            eprintln!("  default: report reserve state; -y: retire snapshots + compact + lower bump");
            eprintln!("  BACK UP FIRST — in-place compaction is not crash-safe.");
            return ExitCode::from(2);
        }
    };
    match run(&path, apply) {
        Ok(code) => ExitCode::from(code as u8),
        Err(e) => { eprintln!("tessera-repack: {e}"); ExitCode::from(1) }
    }
}
