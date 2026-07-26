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
use tessera_tools::{fd_of, make_io, open_file_ro, open_file_rw, rebuild_blob_index, DiskCtx};

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

/// Build ONE tree into [start, cap). Same aliasing discipline as build_all.
/// Returns (root, frontier), or Err if it overran the window — in which case
/// only sectors inside [start, cap) were touched.
fn build_one(io: &tessera_block_io_t, ctxp: *mut DiskCtx, start: u64, cap: u64,
    existed: bool, kind: u8, ksz: u32, vsz: u32, entries: &[(Vec<u8>, Vec<u8>)])
    -> Result<(u64, u64), String>
{
    unsafe { (*ctxp).bump.set(start); (*ctxp).bump_max = cap; }
    let root = write_tree(io, existed, kind, ksz, vsz, entries)?;
    let frontier = unsafe { (*ctxp).bump.get() };
    Ok((root, frontier))
}

/// Maximal runs in [lo, hi) containing NO live node, widest first.
///
/// Staging may only write where a crash cannot hurt: a gap holds nothing the
/// committed superblock references, so an interrupted build there leaves the
/// volume exactly as it was. This is what lets a tree be rebuilt anywhere in
/// the reserve rather than only in the prefix or the tail.
fn free_gaps(live: &[u64], lo: u64, hi: u64) -> Vec<(u64, u64)> {
    let mut s: Vec<u64> = live.iter().copied().filter(|&x| x >= lo && x < hi).collect();
    s.sort_unstable();
    s.dedup();
    let mut out: Vec<(u64, u64)> = Vec::new();
    let mut cur = lo;
    for &n in &s {
        if n > cur { out.push((cur, n)); }
        cur = n + 1;
    }
    if cur < hi { out.push((cur, hi)); }
    out.sort_by_key(|&(a, b)| std::cmp::Reverse(b - a));
    out
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

/// Phase C (#82): bounded per-tree staging.
///
/// Phases A and B rebuild ALL FOUR trees into one window, so they need a
/// contiguous free run as large as the WHOLE live metadata set. That is why a
/// fixed emergency band cannot guarantee the crash-safe path works: the
/// requirement scales with live metadata, and no constant tracks it.
///
/// Moving ONE tree at a time drops the peak requirement to the LARGEST SINGLE
/// TREE, and each committed move frees that tree's old nodes for the next one,
/// so the space available grows as the pass proceeds. Any free gap will do, not
/// just the prefix or the tail — a gap holds nothing the committed SB
/// references, so an interrupted build there is a no-op and the crash-safety
/// argument is unchanged. Each commit publishes one new root alongside the
/// other three unchanged: every intermediate state is a valid volume.
///
/// Returns the number of trees moved.
fn stage_bounded(v: *mut tessera_volume_t, io: &tessera_block_io_t, ctxp: *mut DiskCtx,
    mr_start: u64, ceiling: u64, t: &Trees, next_ino: u64) -> Result<u32, String>
{
    let mut moved = 0u32;

    // Bounded because each iteration STRICTLY lowers the highest live node, and
    // that is a decreasing bounded integer. An earlier version simply moved the
    // first tree that fit anywhere and re-moved the same tree forever without
    // converging; requiring strict improvement is what makes this terminate.
    for _ in 0..64 {
        let (ir, pr, fr, qr, cur_bump) = unsafe {(
            tessera_volume_inode_root(v), tessera_volume_pack_registry_root(v),
            tessera_volume_free_extent_root(v), tessera_volume_quota_tree_root(v),
            tessera_volume_meta_reserve_bump(v),
        )};
        let specs: [(usize, u8, u32, u32, &Vec<(Vec<u8>, Vec<u8>)>, bool, u64); 4] = [
            (0, TESSERA_BTREE_KIND_INODE,    4,  TESSERA_INODE_RECORD_SIZE,   &t.inodes, t.existed.0, ir),
            (1, TESSERA_BTREE_KIND_PACK_REG, 16, TESSERA_REGISTRY_ENTRY_SIZE, &t.packs,  t.existed.1, pr),
            (2, TESSERA_BTREE_KIND_FREE_EXT, 8,  8,                           &t.frees,  t.existed.2, fr),
            (3, TESSERA_BTREE_KIND_QUOTA,    8,  128,                         &t.quotas, t.existed.3, qr),
        ];

        // Per-tree live sets: the victim is whichever tree owns the highest
        // live sector, since that is the one pinning the bump.
        let mut per: Vec<(usize, Vec<u64>)> = Vec::new();
        let mut all: Vec<u64> = Vec::new();
        for (idx, kind, ksz, vsz, _e, existed, root) in specs {
            let mut n = Vec::new();
            if existed { live_nodes(io, root, kind, ksz, vsz, &mut n)?; }
            all.extend_from_slice(&n);
            per.push((idx, n));
        }
        if all.is_empty() { break; }
        let Some(&(victim, _)) = per.iter()
            .filter(|(_, n)| !n.is_empty())
            .max_by_key(|(_, n)| n.iter().copied().max().unwrap_or(0))
            .map(|x| x) else { break };
        let victim_top = per[victim].1.iter().copied().max().unwrap_or(0);

        // Windows that hold nothing the committed SB references, lowest first:
        // a crash inside one is a no-op, which is what keeps this crash-safe.
        let mut gaps = free_gaps(&all, mr_start, ceiling);
        gaps.sort_by_key(|&(a, _)| a);

        let (_, kind, ksz, vsz, entries, existed, _) = specs[victim];
        let mut progressed = false;
        for &(gs, ge) in &gaps {
            if gs >= victim_top { break; }          // cannot improve from here
            let Ok((root, frontier)) = build_one(io, ctxp, gs, ge, existed, kind, ksz, vsz, entries)
                else { continue };
            if frontier == 0 || frontier - 1 >= victim_top { continue; }  // not strictly lower
            let bump = if frontier > cur_bump { frontier } else { cur_bump };
            let mut b = Built { inode: ir, pack: pr, free: fr, quota: qr, bump };
            match victim { 0 => b.inode = root, 1 => b.pack = root,
                           2 => b.free = root, _ => b.quota = root }
            commit(v, &b, next_ino, false)?;
            println!("  crash-safe: moved tree kind={kind} to [{gs},{frontier}) \
(top {victim_top} -> {})", frontier - 1);
            moved += 1;
            progressed = true;
            break;
        }
        if !progressed { break; }
    }

    // Reclaim: the bump may now sit far above the highest live node. Lower it
    // exactly once, at the end — never during the loop, because a bump below a
    // live node is precisely the corruption build_all's aliasing note warns of.
    if moved > 0 {
        let (ir, pr, fr, qr) = unsafe {(
            tessera_volume_inode_root(v), tessera_volume_pack_registry_root(v),
            tessera_volume_free_extent_root(v), tessera_volume_quota_tree_root(v),
        )};
        let mut all: Vec<u64> = Vec::new();
        live_nodes(io, ir, TESSERA_BTREE_KIND_INODE,    4,  TESSERA_INODE_RECORD_SIZE,   &mut all)?;
        live_nodes(io, pr, TESSERA_BTREE_KIND_PACK_REG, 16, TESSERA_REGISTRY_ENTRY_SIZE, &mut all)?;
        live_nodes(io, fr, TESSERA_BTREE_KIND_FREE_EXT, 8,  8,                           &mut all)?;
        live_nodes(io, qr, TESSERA_BTREE_KIND_QUOTA,    8,  128,                         &mut all)?;
        let top = all.iter().copied().max().map(|m| m + 1).unwrap_or(mr_start);
        let b = Built { inode: ir, pack: pr, free: fr, quota: qr, bump: top.max(mr_start) };
        commit(v, &b, next_ino, true)?;
        println!("  crash-safe: bounded staging done — bump lowered to {}", b.bump);
    }
    Ok(moved)
}

fn run(path: &str, apply: bool, force: bool, stage_cap: Option<u64>) -> Result<i32, String> {
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

    // --stage-cap artificially shrinks the usable reserve so the all-at-once
    // path FAILS on demand. Without it the Phase-A refusal only appears on a
    // large, heavily-used, fragmented reserve (the dev root reached it at
    // 408,544 sectors); synthetic volumes always fit, which left the recovery
    // path untestable — and an untestable recovery path is how #57 happened.
    let ceiling = match stage_cap {
        Some(n) => (mr_start + n).min(mr_start + mr_len),
        None    => mr_start + mr_len,
    };
    if stage_cap.is_some() {
        println!("  --stage-cap: usable reserve clamped to [{mr_start},{ceiling}) for testing");
    }

    if force {
        // Legacy in-place overwrite — NOT crash-safe. Only if explicitly asked.
        println!("  --force: in-place overwrite from reserve start (NOT crash-safe — back up first)");
        let b = build_all(&io, ctxp, mr_start, ceiling, &trees)?;
        commit(v, &b, next_ino, true)?;
        rebuild_index_after_repack(&f, &io, ctxp, v, path);
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
                rebuild_index_after_repack(&f, &io, ctxp, v, path);
                report_done(v, mr_start, used, b.bump, path, trees.packs.len());
                return Ok(0);
            }
        }

        // Phase A: stage into headroom above the current bump, commit, loop.
        let headroom = ceiling.saturating_sub(cur_bump);
        let need = live.len() as u64;
        let a = match build_all(&io, ctxp, cur_bump, ceiling, &trees) {
            Ok(a) => a,
            Err(_) => {
                // All-at-once needs sum(trees) of contiguous space. Fall back
                // to moving ONE tree at a time (#82): peak requirement becomes
                // the largest single tree, and any free gap qualifies.
                println!("  all-at-once staging does not fit ({need} live sectors, \
{headroom} free above the bump) — trying bounded per-tree staging");
                let moved = stage_bounded(v, &io, ctxp, mr_start, ceiling, &trees, next_ino)?;
                if moved > 0 {
                    println!("  bounded staging moved {moved} tree(s); re-evaluating");
                    continue;   // outer loop retries Phase B / A with more room
                }
                return Err(format!(
                    "insufficient reserve headroom to repack crash-safely: a live node sits low, \
                     staging the {need} live metadata sector(s) needs roughly that much \
                     contiguous free space, only {headroom} above the bump, and no single tree \
                     fits in any free gap either. Grow the meta-reserve by at least {} sectors \
                     ({} MiB) and re-run; or re-run with --force for an in-place (NOT crash-safe) \
                     compaction — back up first.",
                    need.saturating_sub(headroom).max(1),
                    (need.saturating_sub(headroom).max(1) * 4096).div_ceil(1024 * 1024)));
            }
        };
        commit(v, &a, next_ino, false)?;   // grow-only bump; snapshots now retired
        println!("  crash-safe: staged compacted trees in reserve headroom (snapshots retired)");
    }
    Err("repack did not converge in 3 passes (unexpected)".into())
}

/// Rebuild the blob->pack index that this repack necessarily dropped (#75).
///
/// repack rewrites the meta-reserve, so the old index would point at reused
/// sectors and MUST be dropped — but leaving it absent is a trap: the window
/// between "repack finished" and "operator runs tessera-reindex" is exactly
/// when a reboot becomes a multi-minute apparent hang, because every cold
/// read then scans the whole pack registry.
///
/// Done as a SEPARATE commit after the compaction commits, deliberately. The
/// tree is built in fresh reserve sectors nothing references yet, so if this
/// step fails or is interrupted the volume simply keeps blob_index_root = 0 —
/// exactly the state repack produced before this change, and one that
/// tessera-debug and tessera-fsck now both report explicitly. So the worst
/// case is the old behaviour, never a worse one.
fn rebuild_index_after_repack(f: &std::fs::File, io: &tessera_block_io_t,
    ctxp: *mut DiskCtx, v: *mut tessera_volume_t, path: &str)
{
    let pack_root = unsafe { tessera_volume_pack_registry_root(v) };
    let bump0 = unsafe { tessera_volume_meta_reserve_bump(v) };
    unsafe { (*ctxp).bump.set(bump0); }
    match rebuild_blob_index(f, io, ctxp, pack_root) {
        Ok((root, nblobs, npacks, new_bump)) => {
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
            if rc == 0 {
                println!("  blob index REBUILT: root@{root}, {nblobs} blobs from \
{npacks} packs, {} reserve sectors", new_bump.saturating_sub(bump0));
            } else {
                println!("tessera-repack: index rebuild commit failed (rc={rc}) — \
volume is fine, but run `tessera-reindex {path}` before mounting.");
            }
        }
        Err(e) => {
            println!("tessera-repack: could not rebuild the blob index ({e}) — \
volume is fine, but run `tessera-reindex {path}` before mounting.");
        }
    }
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
    let _ = npacks;
}

fn main() -> ExitCode {
    let args: Vec<_> = std::env::args().collect();
    let mut path = None;
    let mut apply = false;
    let mut force = false;
    let mut stage_cap: Option<u64> = None;
    for a in &args[1..] {
        match a.as_str() {
            "-y" | "--apply" => apply = true,
            "--force"        => { apply = true; force = true; }
            s if s.starts_with("--stage-cap=") => {
                stage_cap = s["--stage-cap=".len()..].parse::<u64>().ok();
                apply = true;
            }
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
            eprintln!("  --stage-cap=N: TESTING — clamp the usable reserve to N sectors so the");
            eprintln!("           all-at-once path fails and the bounded per-tree path is taken");
            return ExitCode::from(2);
        }
    };
    match run(&path, apply, force, stage_cap) {
        Ok(code) => ExitCode::from(code as u8),
        Err(e) => { eprintln!("tessera-repack: {e}"); ExitCode::from(1) }
    }
}
