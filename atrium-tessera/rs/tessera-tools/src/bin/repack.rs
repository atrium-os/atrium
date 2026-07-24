//! tessera-repack — offline, CRASH-SAFE meta-reserve recovery.
//!
//! The metadata-reserve bump pointer only ever advances: under sustained
//! commit churn (esp. with many retained snapshots) it can climb to the reserve
//! ceiling and stay there, leaving the volume permanently dependent on the
//! recycle free-list with no bump headroom. This tool recovers that state
//! OFFLINE (volume must be UNMOUNTED): it retires all retained snapshots, then
//! rewrites the four live metadata trees (inode / pack-registry / free-extent /
//! quota) compactly and lowers the bump to the compacted frontier. Data (packs /
//! file content) is untouched — only metadata b-tree nodes move, and only
//! snapshot *history* is dropped.
//!
//! CRASH-SAFE. Every compacted tree is written ONLY to reserve sectors the
//! current committed superblock does not reference, then sealed with an atomic
//! dual-SB commit_roots. So a crash at any point leaves the committed SB pointing
//! at intact trees — either the original state or a fully-compacted one, never a
//! half-overwritten mix. The device is opened O_SYNC, so every node write is
//! durable before the SB write that references it. Concretely:
//!   - Live node-set: walk the four current roots, collecting every node sector.
//!   - Phase B (fast path / resume): if the compacted trees fit in the free
//!     prefix [reserve_start, lowest_live_node), build them there and commit —
//!     one atomic step, bump reclaimed to the compacted frontier. A build that
//!     doesn't fit touches only free sectors and never commits, so it is a
//!     harmless no-op that falls through to:
//!   - Phase A (stage): build the compacted trees in the free headroom above the
//!     current bump, commit (this retires snapshots and points the roots at the
//!     staged copy). Now the entire low region is unreferenced, so a second
//!     Phase-B build+commit compacts to the reserve start and lowers the bump.
//! A crash between the two commits leaves a consistent (snapshots-retired) volume
//! that a re-run finishes via Phase B. If neither the free prefix nor the
//! headroom can hold the compacted trees (a live node sits low AND the reserve is
//! maxed), the crash-safe path can't run: the tool refuses unless --force is
//! given (old in-place overwrite — NOT crash-safe, back up first).
//!
//! Gated behind -y/--apply; without it, this only reports the current reserve
//! state.

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

/// Collect every node sector of a b-tree (internal + leaf) into `out`.
extern "C" fn collect_cb(ctx: *mut std::ffi::c_void, sector: u64) -> i32 {
    unsafe { (*(ctx as *mut Vec<u64>)).push(sector); }
    0
}
fn live_nodes(io: &tessera_block_io_t, root: u64, kind: u8, ksz: u32, vsz: u32,
    out: &mut Vec<u64>) -> Result<(), String>
{
    if root == 0 { return Ok(()); }
    unsafe {
        let t = tessera_btree_open(io, root, kind, ksz, vsz);
        if t.is_null() { return Err(format!("open tree kind={kind} root={root}")); }
        let ctx = out as *mut Vec<u64> as *mut std::ffi::c_void;
        let rc = tessera_btree_walk_nodes(t, Some(collect_cb), ctx);
        tessera_btree_close(t);
        if rc != 0 { return Err(format!("walk_nodes kind={kind} rc={rc}")); }
    }
    Ok(())
}

/// Create a fresh b-tree (allocating nodes from ctx.bump) and insert all
/// entries. Returns the compacted root. If `existed` is false (old root was 0)
/// the tree is left absent (root 0). Returns Err if the bump allocator hit its
/// cap mid-build — the caller treats that as "did not fit here".
fn write_tree(io: &tessera_block_io_t, existed: bool, kind: u8, ksz: u32, vsz: u32,
    entries: &[(Vec<u8>, Vec<u8>)]) -> Result<u64, String>
{
    if !existed { return Ok(0); }
    unsafe {
        let mut root: u64 = 0;
        let t = tessera_btree_create(io, kind, ksz, vsz, &mut root);
        if t.is_null() { return Err("create tree: bump cap".into()); }
        if !entries.is_empty() {
            // Bulk bottom-up build from the sorted cursor order — far less node
            // churn than per-entry put (which leaks transient split nodes into
            // the bump-only allocator and can inflate the compacted tree past
            // the original). Pack keys contiguously, then values.
            let n = entries.len();
            let mut keys = Vec::with_capacity(n * ksz as usize);
            let mut vals = Vec::with_capacity(n * vsz as usize);
            for (k, v) in entries { keys.extend_from_slice(k); vals.extend_from_slice(v); }
            let mut nr = root;
            let rc = tessera_btree_put_sorted_batch(t, keys.as_ptr(), vals.as_ptr(),
                n as u32, &mut nr);
            if rc != 0 { tessera_btree_close(t); return Err(format!("batch kind={kind} rc={rc}")); }
            root = nr;
        }
        tessera_btree_close(t);
        Ok(root)
    }
}

/// The four live metadata trees, read into RAM.
struct Trees {
    inodes: Vec<(Vec<u8>, Vec<u8>)>,
    packs:  Vec<(Vec<u8>, Vec<u8>)>,
    frees:  Vec<(Vec<u8>, Vec<u8>)>,
    quotas: Vec<(Vec<u8>, Vec<u8>)>,
    existed: (bool, bool, bool, bool),
}

/// Four compacted roots + the bump frontier after building.
struct Built { inode: u64, pack: u64, free: u64, quota: u64, bump: u64 }

/// Build all four compacted trees starting at `start`, capped at `cap`
/// (allocations restricted to [start, cap)). Returns the roots + new frontier,
/// or Err if any tree overran the cap (i.e. the compacted set does not fit in
/// [start, cap)). On Err only sectors in [start, cap) were touched.
///
/// `ctxp` is a raw pointer to the same DiskCtx `io` was built over. We access it
/// only through the raw pointer here — NOT through a `&mut DiskCtx` — because
/// `io`'s alloc callback mutates the same DiskCtx through its own raw pointer
/// during the (opaque) FFI build. A `&mut` binding held across those calls would
/// be `noalias`, letting the compiler cache `bump` and miss disk_alloc's
/// advance (which silently set the committed bump below the live trees).
fn build_all(io: &tessera_block_io_t, ctxp: *mut DiskCtx, start: u64, cap: u64, t: &Trees)
    -> Result<Built, String>
{
    unsafe { (*ctxp).bump.set(start); (*ctxp).bump_max = cap; }
    let inode = write_tree(io, t.existed.0, TESSERA_BTREE_KIND_INODE,   4,  TESSERA_INODE_RECORD_SIZE,   &t.inodes)?;
    let pack  = write_tree(io, t.existed.1, TESSERA_BTREE_KIND_PACK_REG, 16, TESSERA_REGISTRY_ENTRY_SIZE, &t.packs)?;
    let free  = write_tree(io, t.existed.2, TESSERA_BTREE_KIND_FREE_EXT, 8,  8,                           &t.frees)?;
    let quota = write_tree(io, t.existed.3, TESSERA_BTREE_KIND_QUOTA,    8,  128,                         &t.quotas)?;
    let bump = unsafe { (*ctxp).bump.get() };
    Ok(Built { inode, pack, free, quota, bump })
}

/// Atomically commit the four compacted roots, retire snapshots, and set the
/// bump. `bump_exact` lowers the bump (repack reclaim); otherwise it only grows.
fn commit(v: *mut tessera_volume_t, b: &Built, next_ino: u64, bump_exact: bool) -> Result<(), String> {
    let roots = tessera_commit_roots_t {
        inode_root:         b.inode,
        pack_registry_root: b.pack,
        free_extent_root:   b.free,
        quota_tree_root:    b.quota,
        snapshots_root:     0,          // retire all retained snapshots
        meta_reserve_bump:  b.bump,
        next_inode_no:      next_ino,
        blob_index_root:    0,          // reserve is rewritten — index dropped;
                                        // rebuild with tessera-reindex afterward
    };
    let flags = if bump_exact { TESSERA_COMMIT_BUMP_EXACT } else { 0 };
    let rc = unsafe { tessera_volume_commit_roots_ex(v, &roots, flags) };
    if rc != 0 { return Err(format!("commit_roots rc={rc}")); }
    Ok(())
}

fn run(path: &str, apply: bool, force: bool) -> Result<i32, String> {
    let f = if apply {
        open_file_rw(path).map_err(|e| format!("open {path} (rw): {e}"))?
    } else {
        open_file_ro(path).map_err(|e| format!("open {path}: {e}"))?
    };
    let mut ctx = DiskCtx::ro(fd_of(&f));
    let io = make_io(&mut ctx);
    // Access ctx ONLY through this raw pointer past here (see build_all): io's
    // alloc callback aliases ctx, so a &mut binding across the FFI build is UB.
    // Derive it FROM io.ctx (the same pointer/provenance the callbacks use) —
    // NOT a second `&mut ctx` reborrow, which would push a fresh tag above io's
    // and let (*ctxp).bump_max writes invalidate io's pointer (UB under Miri
    // Stacked + Tree Borrows, even though it dodges the LLVM `noalias` miscompile).
    let ctxp = io.ctx as *mut DiskCtx;

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
        println!("  trees crash-safely (staged into unreferenced reserve sectors), lowers the bump.");
        unsafe { tessera_volume_close(v) };
        return Ok(0);
    }

    // Read every entry of the four live trees into RAM, and collect the live
    // node-set (which reserve sectors the current committed SB references) —
    // both BEFORE writing anything, since the rewrite reuses reserve sectors.
    let trees = Trees {
        inodes: read_tree(&io, inode_root, TESSERA_BTREE_KIND_INODE,   4,  TESSERA_INODE_RECORD_SIZE)?,
        packs:  read_tree(&io, pack_root,  TESSERA_BTREE_KIND_PACK_REG, 16, TESSERA_REGISTRY_ENTRY_SIZE)?,
        frees:  read_tree(&io, free_root,  TESSERA_BTREE_KIND_FREE_EXT, 8,  8)?,
        quotas: read_tree(&io, quota_root, TESSERA_BTREE_KIND_QUOTA,    8,  128)?,
        existed: (inode_root != 0, pack_root != 0, free_root != 0, quota_root != 0),
    };
    println!("  read: {} inodes, {} packs, {} free-extents, {} quota domains",
        trees.inodes.len(), trees.packs.len(), trees.frees.len(), trees.quotas.len());

    let ceiling = mr_start + mr_len;

    if force {
        // Legacy in-place overwrite — NOT crash-safe. Only if explicitly asked.
        println!("  --force: in-place overwrite from reserve start (NOT crash-safe — back up first)");
        let b = build_all(&io, ctxp, mr_start, ceiling, &trees)?;
        commit(v, &b, next_ino, true)?;
        report_done(v, mr_start, used, b.bump, path, trees.packs.len());
        return Ok(0);
    }

    // Crash-safe staged compaction. Each pass recomputes the live node-set from
    // the CURRENT committed roots (they move after every commit):
    //   - Phase B: if the compacted trees fit in the free prefix below the
    //     lowest live node, build them there and commit exact — bump reclaimed,
    //     done in one atomic step. A build that overruns the prefix touches only
    //     free sectors and does NOT commit, so it falls through harmlessly.
    //   - Phase A: otherwise stage into the free headroom above the bump and
    //     commit (snapshots retired, roots repointed at the staged copy). Now the
    //     whole low region is unreferenced, so the next pass takes Phase B and
    //     compacts to the reserve start.
    // Converges in at most two commits (Phase A then Phase B); 3 passes is slack.
    for _ in 0..3 {
        // Live node-set + bump from the current on-disk roots (commit updates v).
        let (ir, pr, fr, qr, cur_bump) = unsafe {(
            tessera_volume_inode_root(v), tessera_volume_pack_registry_root(v),
            tessera_volume_free_extent_root(v), tessera_volume_quota_tree_root(v),
            tessera_volume_meta_reserve_bump(v),
        )};
        let mut live: Vec<u64> = Vec::new();
        live_nodes(&io, ir, TESSERA_BTREE_KIND_INODE,   4,  TESSERA_INODE_RECORD_SIZE,   &mut live)?;
        live_nodes(&io, pr, TESSERA_BTREE_KIND_PACK_REG, 16, TESSERA_REGISTRY_ENTRY_SIZE, &mut live)?;
        live_nodes(&io, fr, TESSERA_BTREE_KIND_FREE_EXT, 8,  8,                           &mut live)?;
        live_nodes(&io, qr, TESSERA_BTREE_KIND_QUOTA,    8,  128,                         &mut live)?;
        let first_live = live.iter().copied().min().unwrap_or(ceiling);

        // Phase B: fit entirely below the lowest live node?
        if first_live > mr_start {
            if let Ok(b) = build_all(&io, ctxp, mr_start, first_live, &trees) {
                commit(v, &b, next_ino, true)?;
                println!("  crash-safe: compacted into the free reserve prefix (commit, bump lowered)");
                report_done(v, mr_start, used, b.bump, path, trees.packs.len());
                return Ok(0);
            }
        }

        // Phase A: stage into headroom above the current bump, commit, loop.
        let headroom = ceiling - cur_bump;
        let a = build_all(&io, ctxp, cur_bump, ceiling, &trees).map_err(|_| format!(
            "insufficient reserve headroom to repack crash-safely (a live node sits low \
             AND only {headroom} free sectors above the bump). Grow the reserve, or re-run \
             with --force for an in-place (NOT crash-safe) compaction — back up first."))?;
        commit(v, &a, next_ino, false)?;   // grow-only bump; snapshots now retired
        println!("  crash-safe: staged compacted trees in reserve headroom (snapshots retired)");
    }
    Err("repack did not converge in 3 passes (unexpected)".into())
}

fn report_done(v: *mut tessera_volume_t, mr_start: u64, used: u64, new_bump: u64,
    path: &str, npacks: usize) {
    unsafe { tessera_volume_close(v) };
    let new_used = new_bump - mr_start;
    println!("  compacted:     bump {used} -> {new_used} sectors  (reclaimed {} sectors headroom)",
        used.saturating_sub(new_used));
    /*
     * The blob->pack index was dropped (commit() sets blob_index_root = 0,
     * correctly: the reserve is rewritten under it, and a STALE index returns
     * wrong data while no index is merely slow). But saying so only in a
     * source comment is a trap — without the index every cold read linearly
     * scans the pack registry, and the next boot sits at "Loading kernel..."
     * with the CPU pegged and NO console output for many minutes, which is
     * indistinguishable from a hang. That happened here on a 63929-pack dev
     * root: ~20 minutes to boot, versus 8 seconds for tessera-reindex to
     * rebuild the index. Say it loudly, and scale the wording to the cost.
     */
    println!("tessera-repack: DONE — run tessera-fsck to verify.");
    println!("tessera-repack: NOTE — the blob->pack index was DROPPED (the \
reserve is rewritten, so the old index would point at reused sectors).");
    if npacks >= 10_000 {
        println!("tessera-repack: *** RUN `tessera-reindex {path}` BEFORE \
MOUNTING. *** With {npacks} packs and no index, every cold read scans the \
whole pack registry: the next boot can take MANY MINUTES at \"Loading \
kernel...\" with no output, looking exactly like a hang. Reindexing takes \
seconds.");
    } else {
        println!("tessera-repack: run `tessera-reindex {path}` to rebuild it; \
until then cold reads scan the pack registry and are slow.");
    }
}

fn main() -> ExitCode {
    let args: Vec<_> = std::env::args().collect();
    let mut path = None;
    let mut apply = false;
    let mut force = false;
    for a in &args[1..] {
        match a.as_str() {
            "-y" | "--apply" => apply = true,
            "--force"        => { apply = true; force = true; }
            s if !s.starts_with('-') => path = Some(s.to_string()),
            _ => {}
        }
    }
    let path = match path {
        Some(p) => p,
        None => {
            eprintln!("usage: tessera-repack [-y|--apply] [--force] PATH");
            eprintln!("  offline meta-reserve compaction (volume must be UNMOUNTED)");
            eprintln!("  default: report reserve state");
            eprintln!("  -y:      crash-safe compact — retire snapshots, stage into unreferenced");
            eprintln!("           reserve sectors, lower the bump");
            eprintln!("  --force: in-place overwrite (NOT crash-safe) when the reserve is too");
            eprintln!("           maxed for the staged path — back up first");
            return ExitCode::from(2);
        }
    };
    match run(&path, apply, force) {
        Ok(code) => ExitCode::from(code as u8),
        Err(e) => { eprintln!("tessera-repack: {e}"); ExitCode::from(1) }
    }
}
