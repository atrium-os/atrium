//! tessera-fsck — offline consistency checker for a Tessera volume.
//!
//! Read-only. Validates the on-disk structures a crash or a GC/recovery
//! bug could damage, independent of "do my files reappear" — so it works
//! as the oracle for crash-injection testing as well as an operator tool.
//!
//! v1 checks:
//!   - superblock opens (core validates magic/CRC/HMAC/version) + roots
//!     and zones are in-bounds and non-overlapping
//!   - pack registry: every entry's extent is inside the pack zone and no
//!     two single-extent packs overlap; each pack opens and its blob count
//!     matches; build the set of all blobs present
//!   - inode tree: every record decodes; basic field sanity (nlink, S_IFMT,
//!     size-vs-manifest)
//!   - blob reachability: every inode's manifest + xattr blob exists, and
//!     every blob it transitively references (CHUNK_LIST chunks, CHUNK_TREE /
//!     DIRECTORY_2L / DIRECTORY_BTREE inner-node child manifests) exists —
//!     catches dangling manifests and live blobs reclaimed by a GC/recovery bug
//!   - dirent reachability: walk the dir tree from the root inode; report
//!     orphan inodes (live but unreachable) and dangling dirents (entry →
//!     inode not in the tree)
//!   - free-extent cross-check: the pack zone must be exactly partitioned by
//!     allocated packs + free extents — report free/allocated overlap
//!     (double-state) and leaked sectors (neither)
//!   - nlink verification: files' nlink == incoming dirent count; dirs == 2
//!     (tessera stores no '.'/'..' and doesn't track '..' backlinks)
//!   - multi-extent packs: PEL chain resolved; data extents + PEL sectors
//!     read, opened, blob-verified, and counted in the free-extent partition
//!   - quota accounting: each domain's used_bytes == summed logical size of
//!     the regular files in it
//!
//! Exit: 0 = clean, 1 = problems found, 2 = usage / I/O error.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::os::unix::fs::FileExt;
use std::process::ExitCode;

use tessera_sys::*;
use tessera_tools::{fd_of, make_io, open_file_ro, open_file_rw, DiskCtx, SECTOR_SIZE};

type Hash = [u8; 32];

fn rd_u32(b: &[u8], o: usize) -> u32 { u32::from_le_bytes(b[o..o + 4].try_into().unwrap()) }
fn rd_u64(b: &[u8], o: usize) -> u64 { u64::from_le_bytes(b[o..o + 8].try_into().unwrap()) }
fn rd_hash(b: &[u8], o: usize) -> Hash { let mut h = [0u8; 32]; h.copy_from_slice(&b[o..o + 32]); h }
fn is_null(h: &Hash) -> bool { h.iter().all(|&x| x == 0) }
fn hx(h: &Hash) -> String { h[..6].iter().map(|b| format!("{b:02x}")).collect() }

const S_IFMT: u32 = 0o170000;
const S_IFREG: u32 = 0o100000;

struct Fsck {
    problems: Vec<String>,
    // informational findings (don't fail the check): space-accounting,
    // skipped coverage, etc.
    notes: Vec<String>,
    // every blob hash present in any (single-extent) pack
    all_blobs: HashSet<Hash>,
    // ★ #81: every blob REACHED by the reachability walk (current tree +
    // every retained snapshot). all_blobs minus this is dead space.
    live_blobs: HashSet<Hash>,
    // per pack: (id, sectors occupied, [(blob hash, blob bytes)]) — kept so
    // deadness can be attributed to packs AFTER the live set is known.
    pack_space: Vec<(String, u64, Vec<(Hash, u32)>)>,
    // blob bytes for those that parse as a manifest (for recursion)
    manifests: HashMap<Hash, Vec<u8>>,
    // manifests already reachability-checked (dedup + cycle guard)
    checked: HashSet<Hash>,
    // inode_no → (mode, nlink, manifest_hash) for the reachability + nlink walk
    inode_map: HashMap<u32, (u32, u32, Hash)>,
    // inode_no → (btree key bytes, full 144-byte record) — kept so --repair
    // can patch a field (nlink) and btree_put the record back verbatim.
    inode_raw: HashMap<u32, ([u8; 4], Vec<u8>)>,
    // quota_domain_id → summed logical bytes of regular files in it
    quota_used: HashMap<u64, u64>,
    // ── structured repair actions (populated during the detect passes,
    //    applied by apply_repairs when --repair is set) ──
    // inode records to rewrite (nlink correction): (key, patched value).
    nlink_fixes: Vec<([u8; 4], Vec<u8>)>,
    // quota records to rewrite (used_bytes correction): (key, patched value).
    quota_fixes: Vec<([u8; 8], Vec<u8>)>,
    // the authoritative free-extent set (pack zone minus allocated packs);
    // used to rebuild the free-extent tree when free_tree_dirty.
    free_runs: Vec<(u64, u64)>,
    // set when the on-disk free-extent tree disagrees with reality
    // (leaked sectors, double-state, free overlap / out-of-bounds).
    free_tree_dirty: bool,
    // ── Tier-B structural repairs ──
    // dirents pointing at a non-existent inode: (parent_ino, child_ino, name).
    // --repair republishes the parent directory without them.
    dangling_dirents: Vec<(u32, u64, String)>,
    // live inodes not reachable from the root dir. --repair relinks each
    // into /lost+found as "inode_<n>".
    orphans: Vec<u32>,
    /// Inodes whose OWN manifest blob is missing from every pack. The
    /// directory listing / file content is unrecoverable, so repair resets
    /// them to empty (a valid, mountable state) and lets the next pass
    /// relink the now-unreferenced children into lost+found.
    dangling_manifests: Vec<(u32, u32)>,
    // ── retained-snapshot validation (task #85) ──
    /// When Some(generation), the walk in progress is inside that retained
    /// snapshot, and problem() routes there instead of to the current-tree
    /// list. Snapshot damage is reported SEPARATELY because the repair is
    /// different in kind: you retire the snapshot, you do not patch inodes
    /// inside it (a snapshot is immutable history, not the live tree).
    current_snapshot: Option<u64>,
    /// generation → problems found walking that snapshot's tree.
    snapshot_problems: BTreeMap<u64, Vec<String>>,
    /// generations whose trees reference blobs that are in no pack.
    /// --repair retires these (deletes the snapshot record).
    damaged_snapshots: Vec<u64>,
    // stats
    inodes: u64,
    packs: u64,
    blobs: u64,
    snapshots: u64,
    snapshot_inodes: u64,
}

/// `tessera_errno_t` value for a clean end-of-iteration.
const TESSERA_ENOENT: i32 = -6;

/// Render a `tessera_btree_last_fail` class for an operator (★ #102).
///
/// The distinction is not cosmetic. KIND means the sector holds a valid btree
/// node of another tree, so it was freed and reused and the snapshot rooted
/// there is destroyed — no amount of retrying recovers it. IO means only that
/// this run could not read it, which may well be transient. Reporting both as
/// "damaged" would invite retiring snapshots that are perfectly fine.
fn describe_fail(f: i32, sector: u64, found_kind: u8) -> String {
    match f {
        btree_fail::KIND => format!(
            "sector {sector} now holds a tree of kind {found_kind}, so it was recycled — this snapshot is destroyed, not merely unreadable"),
        btree_fail::HEADER => format!(
            "sector {sector} is not a btree node (bad magic or CRC)"),
        btree_fail::IO => format!(
            "sector {sector} could not be read — this may be transient, so do NOT retire the snapshot on this evidence alone"),
        _ => "unknown reason".to_string(),
    }
}

/// Cap on problems recorded per snapshot: one damaged shared manifest can be
/// referenced by thousands of inodes, and the repair (retire the snapshot) is
/// identical whether it is 1 or 10,000. Keep the report readable.
const SNAP_PROBLEM_CAP: usize = 16;

impl Fsck {
    fn problem(&mut self, s: String) {
        match self.current_snapshot {
            Some(gen) => {
                let v = self.snapshot_problems.entry(gen).or_default();
                if v.len() < SNAP_PROBLEM_CAP {
                    v.push(s);
                } else if v.len() == SNAP_PROBLEM_CAP {
                    v.push("… (further problems in this snapshot suppressed; \
                            the repair is the same regardless of count)".into());
                }
            }
            None => self.problems.push(s),
        }
    }

    /// Verify `hash` exists; if `as_manifest`, parse it and recurse into the
    /// blobs it references. `ctx` labels the referrer for diagnostics.
    fn reach(&mut self, hash: &Hash, as_manifest: bool, ctx: &str, depth: u32) {
        if is_null(hash) || depth > 64 {
            if depth > 64 {
                self.problem(format!("{ctx}: manifest recursion too deep at {}", hx(hash)));
            }
            return;
        }
        if !self.all_blobs.contains(hash) {
            self.problem(format!("{ctx}: references blob {} which is in no pack (dangling)", hx(hash)));
            return;
        }
        // ★ #81: reached => live. Recorded for BOTH manifests and leaf/chunk
        // blobs; the early return below skips the recursion, not the liveness.
        self.live_blobs.insert(*hash);
        if !as_manifest {
            return; // chunk/leaf blob: existence is enough
        }
        if !self.checked.insert(*hash) {
            return; // already walked (shared via dedup)
        }
        let bytes = match self.manifests.get(hash) {
            Some(b) => b.clone(),
            None => {
                self.problem(format!(
                    "{ctx}: blob {} is referenced as a manifest but does not parse as one",
                    hx(hash)
                ));
                return;
            }
        };
        unsafe {
            let p = tessera_manifest_parse(bytes.as_ptr(), bytes.len());
            if p.is_null() {
                self.problem(format!("{ctx}: manifest {} failed to parse", hx(hash)));
                return;
            }
            let kind = tessera_manifest_parser_kind(p);
            let count = tessera_manifest_parser_count(p);
            match kind {
                TESSERA_MFT_CHUNK_LIST => {
                    for i in 0..count {
                        let mut cr: tessera_chunk_record_t = std::mem::zeroed();
                        if tessera_manifest_chunk_at(p, i, &mut cr) == 0 {
                            // ZERO_HOLE chunks have a null hash and no blob
                            let ch = cr.chunk_hash;
                            if !is_null(&ch) {
                                self.reach(&ch, false, &format!("{}[chunk {i}]", hx(hash)), depth + 1);
                            }
                        }
                    }
                }
                TESSERA_MFT_CHUNK_TREE => {
                    for i in 0..count {
                        let mut tr: tessera_tree_record_t = std::mem::zeroed();
                        if tessera_manifest_tree_at(p, i, &mut tr) == 0 {
                            let ch = tr.child_manifest_hash;
                            self.reach(&ch, true, &format!("{}[tree {i}]", hx(hash)), depth + 1);
                        }
                    }
                }
                TESSERA_MFT_DIRECTORY_2L => {
                    for i in 0..count {
                        let mut br: tessera_dir_bucket_record_t = std::mem::zeroed();
                        if tessera_manifest_dir_bucket_at(p, i, &mut br) == 0 {
                            let ch = br.bucket_manifest_hash;
                            self.reach(&ch, true, &format!("{}[bucket {i}]", hx(hash)), depth + 1);
                        }
                    }
                }
                TESSERA_MFT_DIRECTORY_BTREE => {
                    // Inner nodes point at child manifests (recurse); leaf
                    // nodes hold name→inode entries (no blob refs — dirent
                    // reachability is a separate v2 check).
                    if tessera_manifest_dir_btree_is_leaf(p) == 0 {
                        let mut i = 0u32;
                        loop {
                            let mut ch = [0u8; 32];
                            let rc = tessera_manifest_dir_btree_inner_at(p, i, ch.as_mut_ptr());
                            if rc != 0 { break; }
                            self.reach(&ch, true, &format!("{}[btree {i}]", hx(hash)), depth + 1);
                            i += 1;
                        }
                    }
                }
                // INLINE / SYMLINK / DIRECTORY / XATTR_STORE: no blob refs
                _ => {}
            }
            tessera_manifest_parser_free(p);
        }
    }

    /// Collect (child_inode, name) for every dirent under a directory's
    /// manifest, descending DIRECTORY_2L buckets and DIRECTORY_BTREE inner
    /// nodes to their leaves. Read-only (&self); dir manifests live in the
    /// `manifests` map already built in Pass A.
    fn collect_dirents(&self, dir_hash: &Hash, out: &mut Vec<(u64, String)>, depth: u32) {
        if is_null(dir_hash) || depth > 64 {
            return;
        }
        let bytes = match self.manifests.get(dir_hash) {
            Some(b) => b.clone(),
            None => return, // missing dir manifest already flagged by reach()
        };
        unsafe {
            let p = tessera_manifest_parse(bytes.as_ptr(), bytes.len());
            if p.is_null() {
                return;
            }
            let kind = tessera_manifest_parser_kind(p);
            let count = tessera_manifest_parser_count(p);
            match kind {
                TESSERA_MFT_DIRECTORY => {
                    let mut i = 0u32;
                    loop {
                        let (mut ino, mut nm, mut nl) = (0u64, std::ptr::null(), 0u16);
                        if tessera_manifest_dirent_at(p, i, &mut ino, &mut nm, &mut nl) != 0 {
                            break;
                        }
                        out.push((ino, name_str(nm, nl)));
                        i += 1;
                    }
                }
                TESSERA_MFT_DIRECTORY_2L => {
                    for i in 0..count {
                        let mut br: tessera_dir_bucket_record_t = std::mem::zeroed();
                        if tessera_manifest_dir_bucket_at(p, i, &mut br) == 0 {
                            self.collect_dirents(&br.bucket_manifest_hash, out, depth + 1);
                        }
                    }
                }
                TESSERA_MFT_DIRECTORY_BTREE => {
                    if tessera_manifest_dir_btree_is_leaf(p) == 1 {
                        let mut i = 0u32;
                        loop {
                            let (mut ino, mut nm, mut nl) = (0u64, std::ptr::null(), 0u16);
                            if tessera_manifest_dir_btree_leaf_at(p, i, &mut ino, &mut nm, &mut nl) != 0 {
                                break;
                            }
                            out.push((ino, name_str(nm, nl)));
                            i += 1;
                        }
                    } else {
                        let mut i = 0u32;
                        loop {
                            let mut ch = [0u8; 32];
                            if tessera_manifest_dir_btree_inner_at(p, i, ch.as_mut_ptr()) != 0 {
                                break;
                            }
                            self.collect_dirents(&ch, out, depth + 1);
                            i += 1;
                        }
                    }
                }
                _ => {}
            }
            tessera_manifest_parser_free(p);
        }
    }
}

const PEL_MAGIC: u64 = 0x3150_5645_4C45_5054; // "TPELEV01"

/// Walk a multi-extent pack's PEL chain. Returns (data_extents, pel_sectors)
/// or None if a PEL sector is unreadable / has a bad magic. PEL layout:
/// [u64 magic][u32 ver][u32 extent_count][u64 total_len][u64 next_pel]
/// then extent_count × [u64 start][u64 len] starting at offset 32.
fn resolve_pel(f: &std::fs::File, head: u64) -> Option<(Vec<(u64, u64)>, Vec<u64>)> {
    let mut extents = Vec::new();
    let mut pels = Vec::new();
    let mut cur = head;
    let mut guard = 0;
    while cur != 0 && guard < 512 {
        guard += 1;
        pels.push(cur);
        let mut buf = [0u8; SECTOR_SIZE as usize];
        if f.read_at(&mut buf, cur * SECTOR_SIZE).ok()? != SECTOR_SIZE as usize {
            return None;
        }
        if u64::from_le_bytes(buf[0..8].try_into().unwrap()) != PEL_MAGIC {
            return None;
        }
        let ecount = u32::from_le_bytes(buf[12..16].try_into().unwrap()) as usize;
        let next = u64::from_le_bytes(buf[24..32].try_into().unwrap());
        if ecount > 253 {
            return None;
        }
        for i in 0..ecount {
            let o = 32 + i * 16;
            let s = u64::from_le_bytes(buf[o..o + 8].try_into().unwrap());
            let l = u64::from_le_bytes(buf[o + 8..o + 16].try_into().unwrap());
            extents.push((s, l));
        }
        cur = next;
    }
    Some((extents, pels))
}

unsafe fn name_str(p: *const core::ffi::c_char, len: u16) -> String {
    if p.is_null() || len == 0 {
        return String::new();
    }
    let s = std::slice::from_raw_parts(p as *const u8, len as usize);
    String::from_utf8_lossy(s).into_owned()
}

/// One scan pass. Returns (exit_code, repair_actions_applied). With
/// repair=false it only detects. With repair=true it applies fixes, commits,
/// and returns (1, applied>0) so the caller can re-scan; a fully clean scan
/// returns (0, 0).
fn run(path: &str, verbose: bool, repair: bool, repair_budget: u32)
    -> Result<(i32, u32), String> {
    let f = if repair {
        open_file_rw(path).map_err(|e| format!("open {path} (rw): {e}"))?
    } else {
        open_file_ro(path).map_err(|e| format!("open {path}: {e}"))?
    };
    // ctx starts read-only; the bump allocator is armed below once the
    // superblock's meta-reserve extent is known.
    //
    // ⚠ ALIASING: `io`'s alloc/write callbacks mutate this same DiskCtx
    // through the raw pointer stashed in `io.ctx` during the (opaque) FFI
    // repair build. Touch `ctx` ONLY through `ctxp` past here — never through
    // the owned binding — because an access that reaches ctx through the owned
    // tag (a plain field write like `ctx.bump_max = …`, or a `&mut` binding)
    // aliases what disk_alloc mutates through io.ctx: with a live `&mut`
    // binding that is a `noalias` violation the compiler exploits to cache
    // `bump` and miss disk_alloc's advance, committing a meta_reserve_bump
    // BELOW freshly-written repair nodes (see repack.rs and
    // project_tessera_repack_crashsafe). `ctxp` is derived FROM `io.ctx` — the
    // SAME pointer/provenance io's callbacks use — rather than a second
    // `&mut ctx` reborrow, so set/get here and disk_alloc's writes never
    // invalidate each other (verified UB-clean under Miri Stacked + Tree
    // Borrows).
    let mut ctx = DiskCtx::ro(fd_of(&f));
    let io = make_io(&mut ctx);
    let ctxp = io.ctx as *mut DiskCtx;

    let mut v: *mut tessera_volume_t = std::ptr::null_mut();
    let r = unsafe { tessera_volume_open(&io, &mut v) };
    if r != 0 {
        // The superblock itself is unreadable/corrupt — the worst case.
        eprintln!("tessera-fsck: SUPERBLOCK INVALID (tessera_volume_open errno={r}) — both SB copies failed magic/CRC/HMAC/version");
        return Ok((1, 0));
    }

    let total = unsafe { tessera_volume_total_sectors(v) };
    let inode_root = unsafe { tessera_volume_inode_root(v) };
    let pack_root = unsafe { tessera_volume_pack_registry_root(v) };
    let free_root = unsafe { tessera_volume_free_extent_root(v) };
    let pz_start = unsafe { tessera_volume_pack_zone_start(v) };
    let pz_len = unsafe { tessera_volume_pack_zone_length(v) };
    let generation = unsafe { tessera_volume_generation(v) };
    /* volume content-hash algorithm (0=sha256, 1=blake3) — all blob
     * re-hash verification below must use it, not raw sha256 */
    let hash_alg = unsafe { tessera_volume_hash_alg(v) };
    let quota_root = unsafe { tessera_volume_quota_tree_root(v) };
    let snapshots_root = unsafe { tessera_volume_snapshots_root(v) };
    let next_inode_no = unsafe { tessera_volume_next_inode_no(v) };
    let mr_start = unsafe { tessera_volume_meta_reserve_start(v) };
    let mr_len = unsafe { tessera_volume_meta_reserve_length(v) };
    let mr_bump = unsafe { tessera_volume_meta_reserve_bump(v) };

    // Arm the metadata-reserve bump allocator for --repair. COW btree /
    // free-tree nodes written during apply come from [mr_bump, mr_start+mr_len).
    if repair {
        unsafe {
            (*ctxp).bump.set(mr_bump);
            (*ctxp).bump_max = mr_start + mr_len;
        }
    }

    let mut fsck = Fsck {
        problems: Vec::new(),
        notes: Vec::new(),
        all_blobs: HashSet::new(),
        live_blobs: HashSet::new(),
        pack_space: Vec::new(),
        manifests: HashMap::new(),
        checked: HashSet::new(),
        inode_map: HashMap::new(),
        inode_raw: HashMap::new(),
        quota_used: HashMap::new(),
        nlink_fixes: Vec::new(),
        quota_fixes: Vec::new(),
        free_runs: Vec::new(),
        free_tree_dirty: false,
        dangling_dirents: Vec::new(),
        orphans: Vec::new(),
        dangling_manifests: Vec::new(),
        current_snapshot: None,
        snapshot_problems: BTreeMap::new(),
        damaged_snapshots: Vec::new(),
        inodes: 0,
        packs: 0,
        blobs: 0,
        snapshots: 0,
        snapshot_inodes: 0,
    };

    // ── bounds sanity ────────────────────────────────────────────
    for (name, s) in [("inode_root", inode_root), ("pack_registry_root", pack_root),
                      ("free_extent_root", free_root)] {
        if s >= total {
            fsck.problem(format!("{name} sector {s} >= total_sectors {total}"));
        }
    }
    if pz_start + pz_len > total {
        fsck.problem(format!("pack zone [{pz_start}..{}] exceeds total_sectors {total}",
            pz_start + pz_len));
    }

    // ── Pass A: pack registry → blob set + extent overlap ────────
    let mut intervals: Vec<(u64, u64, String)> = Vec::new();
    unsafe {
        let t = tessera_btree_open(&io, pack_root, TESSERA_BTREE_KIND_PACK_REG, 16,
            TESSERA_REGISTRY_ENTRY_SIZE);
        if t.is_null() {
            fsck.problem("could not open pack registry tree".into());
        } else {
            let c = tessera_btree_seek_first(t);
            let cur = c;
            while !cur.is_null() {
                let mut key = [0u8; 16];
                let mut val = [0u8; TESSERA_REGISTRY_ENTRY_SIZE as usize];
                if tessera_btree_cursor_get(cur, key.as_mut_ptr(), val.as_mut_ptr()) != 0 {
                    break;
                }
                fsck.packs += 1;
                let start = rd_u64(&val, 16);
                let len = rd_u64(&val, 24);
                let blob_count = rd_u32(&val, 32);
                let flags = rd_u32(&val, 60);
                let pid = hx(&{ let mut h = [0u8; 32]; h[..16].copy_from_slice(&key); h });

                // Pack body bytes + the sectors it occupies. Single-extent:
                // one contiguous run. Multi-extent: walk the PEL chain for the
                // data extents (the PEL sectors themselves are allocated too).
                let (body, occ): (Option<Vec<u8>>, Vec<(u64, u64, String)>) =
                    if flags & TESSERA_REGISTRY_FLAG_MULTI_EXTENT != 0 {
                        match resolve_pel(&f, start) {
                            Some((exts, pels)) => {
                                if verbose { eprintln!("  pack {pid}: multi-extent, {} data extent(s)", exts.len()); }
                                let mut buf = Vec::new();
                                let mut ok = true;
                                for (s, l) in &exts {
                                    let mut e = vec![0u8; (*l * SECTOR_SIZE) as usize];
                                    if f.read_at(&mut e, s * SECTOR_SIZE).map(|n| n == e.len()).unwrap_or(false) {
                                        buf.extend_from_slice(&e);
                                    } else { ok = false; break; }
                                }
                                let mut o: Vec<(u64, u64, String)> =
                                    exts.iter().map(|(s, l)| (*s, s + l, pid.clone())).collect();
                                for p in &pels { o.push((*p, *p + 1, format!("{pid}:pel"))); }
                                if !ok { fsck.problem(format!("pack {pid}: could not read multi-extent body")); }
                                (if ok { Some(buf) } else { None }, o)
                            }
                            None => {
                                fsck.problem(format!("pack {pid}: invalid/unreadable PEL at sector {start}"));
                                (None, Vec::new())
                            }
                        }
                    } else {
                        let nbytes = (len * SECTOR_SIZE) as usize;
                        let mut buf = vec![0u8; nbytes];
                        let body = if f.read_at(&mut buf, start * SECTOR_SIZE).map(|n| n == nbytes).unwrap_or(false) {
                            Some(buf)
                        } else {
                            fsck.problem(format!("pack {pid}: could not read {nbytes} bytes at sector {start}"));
                            None
                        };
                        (body, vec![(start, start + len, pid.clone())])
                    };

                // record occupied space (Pass D) + bounds-check every extent
                for (s, e, lbl) in &occ {
                    if *s < pz_start || *e > pz_start + pz_len {
                        fsck.problem(format!("pack {lbl} extent [{s}..{e}] outside pack zone [{pz_start}..{}]",
                            pz_start + pz_len));
                    }
                    intervals.push((*s, *e, lbl.clone()));
                }

                // open + enumerate blobs (content hash + manifest capture)
                if let Some(buf) = body {
                    let pr = tessera_pack_open(buf.as_ptr(), buf.len());
                    if pr.is_null() {
                        fsck.problem(format!("pack {pid} failed to open (bad header/CRC)"));
                    } else {
                        let n = tessera_pack_blob_count(pr);
                        if n != blob_count {
                            fsck.problem(format!("pack {pid}: registry blob_count={blob_count} but pack holds {n}"));
                        }
                        let mut this_pack: Vec<(Hash, u32)> = Vec::new();
                        for i in 0..n {
                            let mut bh = [0u8; 32];
                            if tessera_pack_blob_hash_at(pr, i, bh.as_mut_ptr()) == 0 {
                                fsck.all_blobs.insert(bh);
                                fsck.blobs += 1;
                                let mut bytes: *const u8 = std::ptr::null();
                                let mut blen: u32 = 0;
                                if tessera_pack_lookup(pr, bh.as_ptr(), &mut bytes, &mut blen) == 0
                                    && !bytes.is_null() {
                                    let slice = std::slice::from_raw_parts(bytes, blen as usize);
                                    // content-address integrity: re-hash vs claimed hash
                                    let mut got = [0u8; 32];
                                    tessera_content_hash(hash_alg, slice.as_ptr(), slice.len(), got.as_mut_ptr());
                                    if got != bh {
                                        fsck.problem(format!(
                                            "pack {pid}: blob {} content hashes to {} (corrupted data)",
                                            hx(&bh), hx(&got)));
                                    }
                                    let mp = tessera_manifest_parse(slice.as_ptr(), slice.len());
                                    if !mp.is_null() {
                                        fsck.manifests.insert(bh, slice.to_vec());
                                        tessera_manifest_parser_free(mp);
                                    }
                                    this_pack.push((bh, blen));
                                }
                            }
                        }
                        tessera_pack_close(pr);
                        // ★ #81: sectors this pack occupies (data extents +
                        // any PEL sectors), paired with its blob sizes.
                        let secs: u64 = occ.iter().map(|(s, e, _)| e - s).sum();
                        fsck.pack_space.push((pid.clone(), secs, this_pack));
                    }
                }
                if tessera_btree_cursor_next(cur) != 0 { break; }
            }
            if !c.is_null() { tessera_btree_cursor_free(c); }
            tessera_btree_close(t);
        }
    }
    // extent overlap detection
    intervals.sort_by_key(|x| x.0);
    for w in intervals.windows(2) {
        if w[0].1 > w[1].0 {
            fsck.problem(format!("pack extents overlap: {} [..{}] and {} [{}..]",
                w[0].2, w[0].1, w[1].2, w[1].0));
        }
    }

    // ── Pass B: inode tree → sanity + reachability ───────────────
    unsafe {
        let t = tessera_btree_open(&io, inode_root, TESSERA_BTREE_KIND_INODE, 4,
            TESSERA_INODE_RECORD_SIZE);
        if t.is_null() {
            fsck.problem("could not open inode tree".into());
        } else {
            let c = tessera_btree_seek_first(t);
            let cur = c;
            while !cur.is_null() {
                let mut key = [0u8; 4];
                let mut val = [0u8; TESSERA_INODE_RECORD_SIZE as usize];
                if tessera_btree_cursor_get(cur, key.as_mut_ptr(), val.as_mut_ptr()) != 0 {
                    break;
                }
                fsck.inodes += 1;
                let ino = rd_u32(&val, 0);
                let mode = rd_u32(&val, 8);
                let size = rd_u64(&val, 56);
                let nlink = rd_u32(&val, 64);
                let manifest = rd_hash(&val, 72);
                let xattr = rd_hash(&val, 104);

                if nlink == 0 {
                    fsck.problem(format!("inode {ino}: nlink == 0 (live record with no links)"));
                }
                if mode & S_IFMT == 0 {
                    fsck.problem(format!("inode {ino}: mode 0o{mode:o} has no S_IFMT type bits"));
                }
                if mode & S_IFMT == S_IFREG && size > 0 && is_null(&manifest) {
                    fsck.problem(format!("inode {ino}: regular file size {size} but null manifest_hash"));
                }
                let label = format!("inode {ino}");
                // An inode whose own manifest blob is gone can't be walked or
                // read at all (the kmod fails lookups into it with EIO). Record
                // it as repairable rather than merely reporting it dangling.
                if !is_null(&manifest) && !fsck.all_blobs.contains(&manifest) {
                    fsck.dangling_manifests.push((ino, mode));
                }
                fsck.reach(&manifest, true, &label, 0);
                fsck.reach(&xattr, false, &format!("{label} xattr"), 0);
                fsck.inode_map.insert(ino, (mode, nlink, manifest));
                fsck.inode_raw.insert(ino, (key, val.to_vec()));
                // quota is charged on logical file bytes; accumulate per domain
                let qdom = rd_u64(&val, 136);
                if qdom != 0 && mode & S_IFMT == S_IFREG {
                    *fsck.quota_used.entry(qdom).or_insert(0) += size;
                }

                if tessera_btree_cursor_next(cur) != 0 { break; }
            }
            if !c.is_null() { tessera_btree_cursor_free(c); }
            tessera_btree_close(t);
        }
    }

    // ── Pass B2: retained snapshot trees (task #85) ──────────────
    //
    // Until now fsck walked ONLY the current tree, so a manifest referenced
    // solely from a retained snapshot could be missing while fsck reported
    // CLEAN. That is not a corner case: on the dev root the snapshot union
    // more than DOUBLED the reachable set (327k → 751k hashes), so over half
    // the volume's metadata sat in a blind spot — and three missing blobs in
    // there stranded 24 GiB, because the kmod's GC unions snapshots into its
    // live set and (pre-#84) aborted every reclaim cycle over them.
    //
    // Snapshots are immutable history: the only repair is to retire the whole
    // snapshot, so damage is bucketed per generation rather than per inode.
    if snapshots_root != 0 {
        unsafe {
            let st = tessera_btree_open(&io, snapshots_root,
                TESSERA_BTREE_KIND_SNAPSHOT, 8, TESSERA_SNAPSHOT_RECORD_SIZE);
            if st.is_null() {
                fsck.problem("could not open snapshots tree".into());
            } else {
                let sc = tessera_btree_seek_first(st);
                // ★ #102: NULL here is either "no snapshots" or "the
                // snapshots tree root is unreadable". Treating the second as
                // the first reports CLEAN on a volume whose entire snapshot
                // history is gone.
                if sc.is_null() {
                    let mut fs: u64 = 0;
                    let mut fk: u8 = 0;
                    let f = tessera_btree_last_fail(st, &mut fs, &mut fk);
                    if f != btree_fail::NONE {
                        fsck.problem(format!(
                            "snapshots tree at sector {snapshots_root} could not be enumerated ({}) — retained snapshots were NOT checked",
                            describe_fail(f, fs, fk)));
                    }
                }
                while !sc.is_null() {
                    let mut skey = [0u8; 8];
                    let mut sval = [0u8; TESSERA_SNAPSHOT_RECORD_SIZE as usize];
                    if tessera_btree_cursor_get(sc, skey.as_mut_ptr(),
                        sval.as_mut_ptr()) != 0 {
                        fsck.problem("snapshots tree enumeration failed part-way — remaining snapshots were NOT checked".into());
                        break;
                    }
                    let snap_gen = rd_u64(&sval, 0);
                    let snap_inode_root = rd_u64(&sval, 16);
                    fsck.snapshots += 1;

                    // Skip the snapshot that shares the live tree's root: it
                    // is the current tree under another name, already walked.
                    if snap_inode_root != 0 && snap_inode_root != inode_root {
                        if snap_inode_root >= total {
                            fsck.problem(format!(
                                "snapshot generation {snap_gen}: inode_root sector \
                                 {snap_inode_root} >= total_sectors {total}"));
                        } else {
                            let before = fsck.snapshot_problems
                                .get(&snap_gen).map_or(0, |v| v.len());
                            fsck.current_snapshot = Some(snap_gen);
                            let sit = tessera_btree_open(&io, snap_inode_root,
                                TESSERA_BTREE_KIND_INODE, 4,
                                TESSERA_INODE_RECORD_SIZE);
                            if sit.is_null() {
                                fsck.problem(
                                    "inode tree could not be opened".into());
                            } else {
                                let ic = tessera_btree_seek_first(sit);
                                // ★ #102 — THE silent skip. btree_open does
                                // not read the root, so `sit` is non-null even
                                // when the sector was recycled; seek_first is
                                // where that surfaces, as NULL. The old code
                                // then just fell through this loop, walking
                                // zero inodes and recording nothing, so a
                                // volume with destroyed snapshot roots read
                                // CLEAN. The kmod's GC saw the same sectors
                                // and reported them, and the disagreement was
                                // read as the GC being wrong.
                                if ic.is_null() {
                                    let mut fs: u64 = 0;
                                    let mut fk: u8 = 0;
                                    let f = tessera_btree_last_fail(
                                        sit, &mut fs, &mut fk);
                                    if f != btree_fail::NONE {
                                        fsck.problem(format!(
                                            "inode_root sector {snap_inode_root} no longer holds this snapshot's inode tree ({}) — its contents are unrecoverable",
                                            describe_fail(f, fs, fk)));
                                    }
                                }
                                while !ic.is_null() {
                                    let mut k = [0u8; 4];
                                    let mut val =
                                        [0u8; TESSERA_INODE_RECORD_SIZE as usize];
                                    if tessera_btree_cursor_get(ic,
                                        k.as_mut_ptr(), val.as_mut_ptr()) != 0 {
                                        fsck.problem("inode tree enumeration failed part-way — the rest of this snapshot was NOT checked".into());
                                        break;
                                    }
                                    fsck.snapshot_inodes += 1;
                                    let ino = rd_u32(&val, 0);
                                    let manifest = rd_hash(&val, 72);
                                    let xattr = rd_hash(&val, 104);
                                    // `checked` is shared with the current-tree
                                    // walk, so blobs common to both (the vast
                                    // majority) cost nothing here.
                                    fsck.reach(&manifest, true,
                                        &format!("inode {ino}"), 0);
                                    fsck.reach(&xattr, false,
                                        &format!("inode {ino} xattr"), 0);
                                    let nrc = tessera_btree_cursor_next(ic);
                                    if nrc == TESSERA_ENOENT { break; }
                                    if nrc != 0 {
                                        fsck.problem(format!(
                                            "inode tree enumeration aborted (rc {nrc}) — the rest of this snapshot was NOT checked"));
                                        break;
                                    }
                                }
                                if !ic.is_null() { tessera_btree_cursor_free(ic); }
                                tessera_btree_close(sit);
                            }
                            fsck.current_snapshot = None;
                            let after = fsck.snapshot_problems
                                .get(&snap_gen).map_or(0, |v| v.len());
                            if after > before {
                                fsck.damaged_snapshots.push(snap_gen);
                            }
                        }
                    }
                // ENOENT is the ONLY clean end of iteration; any other
                // status truncated the walk (★ #102).
                let nrc = tessera_btree_cursor_next(sc);
                    if nrc == TESSERA_ENOENT { break; }
                    if nrc != 0 {
                        fsck.problem(format!(
                            "snapshots tree enumeration aborted (rc {nrc}) — remaining snapshots were NOT checked"));
                        break;
                    }
                }
                if !sc.is_null() { tessera_btree_cursor_free(sc); }
                tessera_btree_close(st);
            }
        }
    }

    // ── Pass C: dirent → inode reachability (orphans + dangling dirents) ─
    const S_IFDIR: u32 = 0o040000;
    let root = TESSERA_INODE_ROOT_DIR;
    let mut reachable: HashSet<u32> = HashSet::new();
    if !fsck.inode_map.contains_key(&root) {
        fsck.problem(format!("root inode {root} missing from inode tree"));
    } else {
        let mut stack = vec![root];
        reachable.insert(root);
        let mut dangling = Vec::new();
        let mut dangling_struct: Vec<(u32, u64, String)> = Vec::new();
        // incoming dirent references per inode (for nlink verification)
        let mut links: HashMap<u32, u32> = HashMap::new();
        while let Some(ino) = stack.pop() {
            let (mode, _nlink, manifest) = match fsck.inode_map.get(&ino) { Some(x) => *x, None => continue };
            if mode & S_IFMT != S_IFDIR { continue; }
            let mut ents = Vec::new();
            fsck.collect_dirents(&manifest, &mut ents, 0);
            for (child, name) in ents {
                if name == "." || name == ".." { continue; }
                let c = child as u32;
                if !fsck.inode_map.contains_key(&c) {
                    dangling.push(format!("dangling dirent: inode {ino} entry '{name}' -> inode {child} (not in inode tree)"));
                    dangling_struct.push((ino, child, name));
                    // a dangling entry will be removed by repair, so it does
                    // NOT count toward the child's incoming links.
                } else {
                    *links.entry(c).or_insert(0) += 1;
                    if reachable.insert(c) {
                        stack.push(c);
                    }
                }
            }
        }
        for d in dangling { fsck.problem(d); }
        fsck.dangling_dirents = dangling_struct;
        let mut orphans: Vec<u32> = fsck.inode_map.keys().copied()
            .filter(|k| !reachable.contains(k)).collect();
        orphans.sort_unstable();
        for o in &orphans {
            fsck.problem(format!("orphan inode {o} (not reachable from the root dir)"));
        }
        fsck.orphans = orphans.clone();

        // ── ★ #81: how much space would COMPACTION actually recover? ──
        //
        // #81's committed fix (6cc3734) frees a pack only when EVERY blob in
        // it is dead. Its own caveat 2 notes that with ~62-blob aggregation
        // the odds of that are ~0.85^62, so the real prize is packs that MIX
        // live and dead blobs — recoverable only by rewriting the survivors
        // into a new pack. Nobody had measured how big that prize is, so the
        // compactor could not be justified or dismissed. This measures it.
        //
        // Read-only, and it reuses two things the walk already produced: the
        // set of blobs actually reached (current tree UNIONED with every
        // retained snapshot — so snapshot-pinned data counts as LIVE, which
        // is correct: GC must not free it and compaction cannot either), and
        // each pack's blob sizes.
        {
            let mut fully_dead = (0u64, 0u64);   // (packs, sectors)
            let mut fully_live = (0u64, 0u64);
            let mut mixed = (0u64, 0u64);        // (packs, sectors)
            let mut mixed_dead_bytes = 0u64;
            let mut mixed_live_bytes = 0u64;
            for (_pid, secs, blobs) in &fsck.pack_space {
                if blobs.is_empty() { continue; }
                let mut dead_b = 0u64;
                let mut live_b = 0u64;
                for (h, len) in blobs {
                    if fsck.live_blobs.contains(h) { live_b += *len as u64; }
                    else { dead_b += *len as u64; }
                }
                if dead_b == 0 { fully_live.0 += 1; fully_live.1 += secs; }
                else if live_b == 0 { fully_dead.0 += 1; fully_dead.1 += secs; }
                else {
                    mixed.0 += 1; mixed.1 += secs;
                    mixed_dead_bytes += dead_b;
                    mixed_live_bytes += live_b;
                }
            }
            let mib = |sectors: u64| sectors * SECTOR_SIZE / (1024 * 1024);
            let bmib = |b: u64| b / (1024 * 1024);
            // ★ #102: the DIRECT comparison against the kmod's pass-1 line
            // ("gc pass1 — N live hashes"). If these two live-set sizes
            // disagree, the components disagree about liveness itself; if
            // they match, the disagreement is in pack-level accounting.
            fsck.notes.push(format!(
                "space: live set = {} of {} blobs reachable (compare with the \
                 kmod's 'gc pass1 — N live hashes')",
                fsck.live_blobs.len(), fsck.all_blobs.len()));
            fsck.notes.push(format!(
                "space: {} packs fully live ({} MiB), {} fully dead ({} MiB, \
                 reclaimable by GC today), {} MIXED ({} MiB holding {} MiB \
                 dead + {} MiB live)",
                fully_live.0, mib(fully_live.1),
                fully_dead.0, mib(fully_dead.1),
                mixed.0, mib(mixed.1), bmib(mixed_dead_bytes), bmib(mixed_live_bytes)));
            if mixed.0 > 0 {
                fsck.notes.push(format!(
                    "space: COMPACTION would recover ~{} MiB by rewriting {} MiB \
                     of survivors out of {} mixed packs (#81 caveat 2)",
                    bmib(mixed_dead_bytes), bmib(mixed_live_bytes), mixed.0));
            }
        }
        // nlink verification: files = incoming dirent count; dirs = 2
        // (tessera stores no '.'/'..' and doesn't track '..' backlinks, so a
        // directory's nlink is fixed at 2 = parent's entry + implicit '.').
        let mut ino_sorted: Vec<u32> = fsck.inode_map.keys().copied().collect();
        ino_sorted.sort_unstable();
        let mut nlink_problems = Vec::new();
        for ino in ino_sorted {
            let (mode, nlink, _) = fsck.inode_map[&ino];
            let is_dir = mode & S_IFMT == S_IFDIR;
            let expected = if is_dir { 2 } else { *links.get(&ino).unwrap_or(&0) };
            // root has no parent dirent but is still nlink 2; only check
            // reachable inodes (orphans already reported separately).
            if reachable.contains(&ino) && nlink != expected {
                nlink_problems.push(format!(
                    "inode {ino}: nlink {nlink} but {expected} {} reference(s)",
                    if is_dir { "expected (dir)" } else { "dirent" }));
                // repair: patch nlink (offset 64, u32 LE) into the record.
                if let Some((key, raw)) = fsck.inode_raw.get(&ino) {
                    let mut patched = raw.clone();
                    patched[64..68].copy_from_slice(&expected.to_le_bytes());
                    fsck.nlink_fixes.push((*key, patched));
                }
            }
        }
        for p in nlink_problems { fsck.problem(p); }
    }

    // ── Pass D: free-extent vs allocation cross-check ────────────
    // The pack zone must be exactly partitioned by allocated single-extent
    // packs and free-extent-tree extents: no sector both free and packed
    // (double-state → next alloc clobbers a live pack), none lost (leak).
    // Skipped/approximate when multi-extent packs are present (their sectors
    // aren't in `intervals`).
    {
        let mut free: Vec<(u64, u64)> = Vec::new();
        if free_root != 0 {
            unsafe {
                let t = tessera_btree_open(&io, free_root, TESSERA_BTREE_KIND_FREE_EXT, 8, 8);
                if t.is_null() {
                    fsck.problem("could not open free-extent tree".into());
                } else {
                    let c = tessera_btree_seek_first(t);
                    let cur = c;
                    while !cur.is_null() {
                        let mut k = [0u8; 8];
                        let mut val = [0u8; 8];
                        if tessera_btree_cursor_get(cur, k.as_mut_ptr(), val.as_mut_ptr()) != 0 {
                            break;
                        }
                        free.push((u64::from_le_bytes(k), u64::from_le_bytes(val)));
                        if tessera_btree_cursor_next(cur) != 0 { break; }
                    }
                    if !c.is_null() { tessera_btree_cursor_free(c); }
                    tessera_btree_close(t);
                }
            }
        }
        let pz_end = pz_start + pz_len;
        // free extents in-bounds + free-vs-free overlap
        free.sort_by_key(|x| x.0);
        for (s, l) in &free {
            if *s < pz_start || s + l > pz_end {
                fsck.problem(format!("free extent [{s}..{}] outside pack zone [{pz_start}..{pz_end}]", s + l));
                fsck.free_tree_dirty = true;
            }
        }
        for w in free.windows(2) {
            if w[0].0 + w[0].1 > w[1].0 {
                fsck.problem(format!("free extents overlap: [{}..{}] and [{}..]",
                    w[0].0, w[0].0 + w[0].1, w[1].0));
                fsck.free_tree_dirty = true;
            }
        }
        // merge packs (from Pass A) + free, detect cross-type overlap + gaps
        let mut all: Vec<(u64, u64, bool)> = Vec::new(); // (start, end, is_free)
        for (s, l) in &free { all.push((*s, s + l, true)); }
        for (s, e, _) in &intervals { all.push((*s, *e, false)); }
        all.sort_by_key(|x| x.0);
        for w in all.windows(2) {
            if w[0].1 > w[1].0 && w[0].2 != w[1].2 {
                let (fr, pk) = if w[0].2 { (w[0], w[1]) } else { (w[1], w[0]) };
                fsck.problem(format!(
                    "free extent [{}..{}] overlaps allocated pack [{}..{}] (double-state)",
                    fr.0, fr.1, pk.0, pk.1));
                fsck.free_tree_dirty = true;
            }
        }
        // coverage/leak: packs (incl. multi-extent data extents + PEL sectors)
        // plus free extents must tile the whole pack zone.
        let mut cursor = pz_start;
        let mut leaked = 0u64;
        // Record the gaps themselves, not just the total. A count alone cannot
        // distinguish one big abandoned run (an allocation that died partway)
        // from thousands of single-sector crumbs (an off-by-one at every
        // free), and those have opposite fixes.
        let mut gaps: Vec<(u64, u64)> = Vec::new();
        for (s, e, _) in &all {
            if *s > cursor { leaked += *s - cursor; gaps.push((cursor, *s)); }
            if *e > cursor { cursor = *e; }
        }
        if cursor < pz_end { leaked += pz_end - cursor; gaps.push((cursor, pz_end)); }
        if leaked > 0 {
            // Informational, not a failure: untracked space is a space-
            // efficiency issue (not corruption), and can arise from
            // allocator behaviour under fragmentation. Double-state overlap
            // above IS a hard problem. --repair reclaims it when rebuilding
            // the free tree.
            fsck.notes.push(format!("{leaked} pack-zone sector(s) neither allocated nor free (leaked space)"));
            let mut hist: BTreeMap<u64, u64> = BTreeMap::new();
            for (a, b) in &gaps { *hist.entry(b - a).or_insert(0) += 1; }
            let shape: Vec<String> = hist.iter().rev().take(6)
                .map(|(len, n)| format!("{n}x{len}sec")).collect();
            fsck.notes.push(format!(
                "  leak shape: {} gap(s), sizes: {}", gaps.len(),
                shape.join(" ")));
            if verbose {
                for (a, b) in gaps.iter().take(24) {
                    fsck.notes.push(format!(
                        "  leaked extent [{a}..{b}] ({} sectors)", b - a));
                }
            }
            fsck.free_tree_dirty = true;
        }

        // Authoritative free set for --repair: the pack zone minus every
        // allocated pack extent (the pack registry is the source of truth
        // for what is in use). Rebuilding the free-extent tree from this
        // complement fixes leaked space, double-state, and free overlap in
        // one shot. Computed regardless of dirtiness; applied only when
        // free_tree_dirty. NB: `intervals` already includes multi-extent
        // data extents + PEL sectors (Pass A), so the complement is exact.
        let mut alloc: Vec<(u64, u64)> =
            intervals.iter().map(|(s, e, _)| (*s, *e)).collect();
        alloc.sort_by_key(|x| x.0);
        let mut cur = pz_start;
        for (s, e) in &alloc {
            let s = (*s).max(pz_start);
            let e = (*e).min(pz_end);
            if s > cur { fsck.free_runs.push((cur, s - cur)); }
            if e > cur { cur = e; }
        }
        if cur < pz_end { fsck.free_runs.push((cur, pz_end - cur)); }
    }

    // ── Pass E: quota accounting ─────────────────────────────────
    // Each domain's used_bytes must equal the summed logical size of the
    // regular files in it (quota is charged on file bytes via vop_write).
    // quota_root read up-front (also needed by apply_repairs).
    if quota_root != 0 {
        unsafe {
            let t = tessera_btree_open(&io, quota_root, TESSERA_BTREE_KIND_QUOTA, 8, 128);
            if t.is_null() {
                fsck.problem("could not open quota tree".into());
            } else {
                let c = tessera_btree_seek_first(t);
                let cur = c;
                while !cur.is_null() {
                    let mut k = [0u8; 8];
                    let mut val = [0u8; 128];
                    if tessera_btree_cursor_get(cur, k.as_mut_ptr(), val.as_mut_ptr()) != 0 {
                        break;
                    }
                    let domain_id = rd_u64(&val, 0);
                    let used = rd_u64(&val, 24);
                    let computed = *fsck.quota_used.get(&domain_id).unwrap_or(&0);
                    if used != computed {
                        fsck.problem(format!(
                            "quota domain {domain_id}: used_bytes={used} but regular-file sizes sum to {computed}"));
                        // repair: patch used_bytes (offset 24, u64 LE).
                        let mut patched = val.to_vec();
                        patched[24..32].copy_from_slice(&computed.to_le_bytes());
                        fsck.quota_fixes.push((k, patched));
                    }
                    if tessera_btree_cursor_next(cur) != 0 { break; }
                }
                if !c.is_null() { tessera_btree_cursor_free(c); }
                tessera_btree_close(t);
            }
        }
    }

    // A volume with no blob→pack index is perfectly CONSISTENT, so this is
    // an advisory rather than a problem — same category as leaked space.
    // But it is worth surfacing on the routine health check, because the
    // symptom is otherwise invisible until the first cold read: every read
    // linearly scans the pack registry, and on a large volume the next boot
    // sits at "Loading kernel..." for many minutes looking exactly like a
    // hang. tessera-repack drops the index and warns, but only the person
    // who ran repack ever sees that warning (#75).
    if unsafe { tessera_volume_blob_index_root(v) } == 0 && fsck.packs > 0 {
        fsck.notes.push(format!(
            "no blob->pack index (blob_index_root=0): every cold read scans \
             all {} packs — run `tessera-reindex {path}`{}",
            fsck.packs,
            if fsck.packs >= 10_000 {
                "  *** BEFORE MOUNTING: at this pack count the next boot can \
                 take many minutes with no output ***"
            } else { "" }));
    }

    // ── report ───────────────────────────────────────────────────
    println!("tessera-fsck: {path}");
    println!("  generation:   {generation}");
    println!("  inodes:       {}", fsck.inodes);
    println!("  packs:        {} ({} blobs, {} parse as manifests)",
        fsck.packs, fsck.blobs, fsck.manifests.len());
    if fsck.snapshots > 0 {
        println!("  snapshots:    {} retained ({} inodes walked)",
            fsck.snapshots, fsck.snapshot_inodes);
    }
    for n in &fsck.notes {
        println!("  NOTE: {n}");
    }
    // Snapshot damage is reported in its own section, and never folded into
    // the current-tree problem list: the volume can be perfectly healthy for
    // every live read while a retained snapshot is unreadable, and the repair
    // (retire the snapshot) is unrelated to any current-tree fix.
    if !fsck.snapshot_problems.is_empty() {
        let total_snap: usize = fsck.snapshot_problems.values().map(|v| v.len()).sum();
        println!("  SNAPSHOT DAMAGE: {} problem(s) across {} retained snapshot(s):",
            total_snap, fsck.snapshot_problems.len());
        for (gen, probs) in &fsck.snapshot_problems {
            println!("    snapshot generation {gen}:");
            for p in probs {
                println!("      - {p}");
            }
        }
        println!("    → These blobs are gone; the snapshots referencing them cannot be");
        println!("      fully read back. The live filesystem is unaffected. Repair is to");
        println!("      RETIRE the affected snapshots (tessera-fsck --repair does this).");
    }
    if fsck.problems.is_empty() && fsck.snapshot_problems.is_empty() {
        println!("  result:       CLEAN — no inconsistencies found");
        unsafe { tessera_volume_close(v); }
        return Ok((0, 0));
    }
    if fsck.problems.is_empty() {
        // Historically this printed CLEAN — the #85 bug exactly.
        println!("  result:       current tree CLEAN; {} damaged snapshot(s)",
            fsck.snapshot_problems.len());
    } else {
        println!("  result:       {} PROBLEM(S) FOUND:", fsck.problems.len());
        for p in &fsck.problems {
            println!("    - {p}");
        }
    }

    if !repair {
        unsafe { tessera_volume_close(v); }
        return Ok((1, 0));
    }

    // ── --repair: apply fixes and commit a new superblock. The caller
    //    (run_to_fixpoint) re-scans to verify and to catch second-order
    //    fixes — e.g. an orphan relinked into lost+found this pass has its
    //    nlink recounted next pass. ──
    let roots = RepairRoots {
        inode_root, pack_registry_root: pack_root, free_extent_root: free_root,
        quota_tree_root: quota_root, snapshots_root, next_inode_no,
        pz_start, pz_len, hash_alg, mr_start, mr_len,
    };
    println!("\ntessera-fsck: --repair — applying fixes…");
    // #96: bound the work per pass so a space-limited repair commits partial
    // progress instead of failing atomically. The fixpoint loop re-runs until
    // it converges or stops making progress.
    let mut truncated = false;
    let (applied, new_roots) = match apply_repairs(&f, &io, &fsck, &roots, verbose,
        repair_budget, &mut truncated)
    {
        Ok(x) => x,
        Err(e) => { unsafe { tessera_volume_close(v); } return Err(format!("repair aborted: {e}")); }
    };
    if applied == 0 {
        println!("tessera-fsck: nothing safely repairable by this tool (superblock-level or unrecoverable damage) — see problems above");
        unsafe { tessera_volume_close(v); }
        return Ok((1, 0));
    }
    // Seal a new superblock pointing at the repaired roots. The bump has
    // advanced past every reserve sector the rewrites consumed — read it
    // back through the raw pointer (see the aliasing note above), never the
    // owned `ctx` binding, so we observe disk_alloc's actual advance.
    let commit = tessera_commit_roots_t {
        inode_root:         new_roots.inode_root,
        pack_registry_root: new_roots.pack_registry_root,
        free_extent_root:   new_roots.free_extent_root,
        quota_tree_root:    new_roots.quota_tree_root,
        snapshots_root:     new_roots.snapshots_root,
        meta_reserve_bump:  unsafe { (*ctxp).bump.get() },
        next_inode_no:      new_roots.next_inode_no,
        // repair appends to the bump (never rewrites the reserve start), so the
        // blob→pack index nodes survive — preserve its root.
        blob_index_root:    unsafe { tessera_volume_blob_index_root(v) },
    };
    let rc = unsafe { tessera_volume_commit_roots(v, &commit) };
    unsafe { tessera_volume_close(v); }
    if rc != 0 {
        return Err(format!("commit_roots failed: rc={rc}"));
    }
    println!("tessera-fsck: applied {applied} repair action(s); committed generation {}",
        generation + 1);
    if truncated {
        println!("tessera-fsck: this pass was CAPPED (budget {repair_budget}) or ran out of \
reserve — the repairs above are committed and durable; re-run to continue.");
    }
    Ok((1, applied))
}

/// Drive --repair to a fixpoint: repeatedly scan + apply until the volume
/// verifies clean, no further progress is possible, or a pass cap is hit.
/// Multiple passes are needed because some repairs expose others (relinking
/// an orphan changes its link count; republishing a directory changes the
/// free set). Returns the process exit code.
/// Drive --repair to a fixpoint. Each pass now applies at most REPAIR_BUDGET
/// actions and COMMITS them (#96), so a volume too damaged to repair in one
/// go still converges over several passes — and if it runs out of reserve
/// entirely, everything achieved up to that point is durable rather than
/// discarded. Pass cap is generous because each pass is deliberately small.
fn run_to_fixpoint(path: &str, verbose: bool) -> Result<i32, String> {
    const MAX_PASSES: u32 = 64;
    const REPAIR_BUDGET: u32 = 256;
    let mut total = 0u32;
    for pass in 0..MAX_PASSES {
        if pass > 0 { println!("\n──── repair pass {} ────", pass + 1); }
        let (exit, applied) = run(path, verbose, true, REPAIR_BUDGET)?;
        total += applied;
        if exit == 0 {
            if pass > 0 {
                println!("\ntessera-fsck: REPAIR SUCCESSFUL — volume is now clean \
({total} action(s) over {} pass(es))", pass + 1);
            }
            return Ok(0);
        }
        if applied == 0 {
            println!("\ntessera-fsck: REPAIR INCOMPLETE after {total} action(s) — the \
remaining problems are not safely repairable by this tool (or the reserve is \
exhausted; run tessera-repack to reclaim it, then re-run).");
            return Ok(1);
        }
    }
    println!("\ntessera-fsck: still making progress after {MAX_PASSES} passes \
({total} action(s) applied and committed) — re-run to continue.");
    Ok(1)
}

/// Roots + geometry the repair passes need, snapshotted from the volume
/// before mutation.
struct RepairRoots {
    inode_root: u64,
    pack_registry_root: u64,
    free_extent_root: u64,
    quota_tree_root: u64,
    snapshots_root: u64,
    next_inode_no: u64,
    pz_start: u64,
    pz_len: u64,
    hash_alg: u32,
    mr_start: u64,
    mr_len: u64,
}

/// The roots after repair (only the moved ones differ from RepairRoots).
struct NewRoots {
    inode_root: u64,
    pack_registry_root: u64,
    free_extent_root: u64,
    quota_tree_root: u64,
    snapshots_root: u64,
    next_inode_no: u64,
}

/// Apply the structured repairs collected during the detect passes and
/// return (actions applied, new roots). Every write goes through the
/// O_SYNC device fd, so on return all rewritten structures are durable;
/// the caller then seals a new superblock via tessera_volume_commit_roots.
///
/// Ordering: Tier-B structural repairs (which mint inodes / republish
/// directories / add packs) run first so Tier-A's nlink recount and the
/// free-tree rebuild observe the final inode tree and pack registry.
///
/// ★ task #96: BOUNDED and NON-ATOMIC on purpose.
///
/// Repair consumption was measured at ~0.65-0.8 meta-reserve sectors PER
/// PROBLEM — O(damage), not the "few btree_deletes" the emergency band was
/// sized against. A 1696-problem volume consumed 1361 sectors against a
/// 256-sector band, and one 64 MiB case failed with applied=0: it did
/// NOTHING, losing every repair it had computed.
///
/// So: apply at most `budget` actions, then stop at a unit boundary and let
/// the caller COMMIT what was done. Every unit (one directory republish, one
/// inode put) leaves the trees individually valid, so a commit after K units
/// is a consistent volume — the same argument that makes repack's staged
/// commits safe. And an allocation failure mid-way now TRUNCATES rather than
/// propagating, so partial progress survives instead of being discarded.
/// Re-running continues from the committed state.
fn apply_repairs(
    f: &std::fs::File,
    io: &tessera_block_io_t,
    fsck: &Fsck,
    r: &RepairRoots,
    verbose: bool,
    budget: u32,
    truncated: &mut bool,
) -> Result<(u32, NewRoots), String> {
    let mut new = NewRoots {
        inode_root: r.inode_root,
        pack_registry_root: r.pack_registry_root,
        free_extent_root: r.free_extent_root,
        quota_tree_root: r.quota_tree_root,
        snapshots_root: r.snapshots_root,
        next_inode_no: r.next_inode_no,
    };
    let mut applied = 0u32;
    let _ = (r.pz_start, r.pz_len, r.mr_start, r.mr_len);

    // Local mutable free set (authoritative complement from Pass D). Tier-B
    // pack publishes carve from it; the Tier-A.3 rebuild flushes the final
    // set, so new packs never end up double-stated against the free tree.
    let mut free: Vec<(u64, u64)> = fsck.free_runs.clone();
    let mut free_dirty = fsck.free_tree_dirty;

    // ── Tier B: structural repairs (republish directories) ──
    // Collect every directory mutation first, keyed by dir inode, so each
    // directory is republished exactly once even when it needs several edits
    // (e.g. the root dir both loses a dangling entry and gains lost+found).
    const S_IFDIR: u32 = 0o040000;
    #[derive(Default)]
    struct DirMod {
        remove: std::collections::HashSet<(u64, String)>,
        add: Vec<(String, u64)>,
        from_empty: bool, // true for a freshly-minted (lost+found) directory
    }
    let mut mods: HashMap<u32, DirMod> = HashMap::new();

    for (parent, child, name) in &fsck.dangling_dirents {
        mods.entry(*parent).or_default().remove.insert((*child, name.clone()));
    }

    // nlink==0 orphans are unlinked-but-still-open files caught by a crash
    // (the kmod keeps the record at unlink and deletes it at last close —
    // POSIX; a crash in between leaves this exact signature). The file WAS
    // deleted: FREE the record rather than resurrecting a dead temp file in
    // lost+found. Orphans with nlink > 0 are genuine losses — relink those.
    let (free_orphans, relink_orphans): (Vec<u32>, Vec<u32>) =
        fsck.orphans.iter().copied().partition(|o|
            fsck.inode_map.get(o).map(|x| x.1 == 0).unwrap_or(false));
    if !free_orphans.is_empty() {
        unsafe {
            let t = tessera_btree_open(io, new.inode_root, TESSERA_BTREE_KIND_INODE,
                4, TESSERA_INODE_RECORD_SIZE);
            if t.is_null() { return Err("open inode tree for orphan free".into()); }
            let mut root = new.inode_root;
            for o in &free_orphans {
                let key = o.to_be_bytes();
                if tessera_btree_delete(t, key.as_ptr(), &mut root) != 0 {
                    tessera_btree_close(t);
                    return Err(format!("btree_delete(inode {o}) during orphan free"));
                }
                applied += 1;
            }
            tessera_btree_close(t);
            new.inode_root = root;
        }
        if verbose { eprintln!("  freed {} nlink=0 orphan(s) (unlinked-open at crash)", free_orphans.len()); }
    }
    if !relink_orphans.is_empty() {
        // Resolve (or mint) /lost+found, then link each orphan into it.
        let root = TESSERA_INODE_ROOT_DIR;
        let root_manifest = fsck.inode_map.get(&root).map(|x| x.2).unwrap_or([0u8; 32]);
        let mut root_ents = Vec::new();
        fsck.collect_dirents(&root_manifest, &mut root_ents, 0);
        let existing = root_ents.iter().find(|(_, n)| n == "lost+found").map(|(c, _)| *c as u32);
        let laf_ino = match existing {
            Some(l) if fsck.inode_map.get(&l).map(|x| x.0 & S_IFMT == S_IFDIR).unwrap_or(false) => l,
            _ => {
                // Mint a new lost+found directory inode and link it into root.
                let l = new.next_inode_no as u32;
                new.next_inode_no += 1;
                mods.entry(root).or_default().add.push(("lost+found".to_string(), l as u64));
                mods.entry(l).or_default().from_empty = true;
                l
            }
        };
        let m = mods.entry(laf_ino).or_default();
        for o in &relink_orphans {
            // Don't relink lost+found itself if it somehow appears orphaned.
            if *o == laf_ino { continue; }
            m.add.push((format!("inode_{o}"), *o as u64));
        }
    }

    // Publish each modified directory once.
    let mut dirs: Vec<u32> = mods.keys().copied().collect();
    dirs.sort_unstable();
    for dir_ino in dirs {
        let m = &mods[&dir_ino];
        // Base entry list: current on-disk entries, or empty for a minted dir.
        if applied >= budget { *truncated = true; break; }
        let mut ents: Vec<(String, u64)> = if m.from_empty {
            Vec::new()
        } else {
            let manifest = fsck.inode_map.get(&dir_ino).map(|x| x.2).unwrap_or([0u8; 32]);
            let mut e = Vec::new();
            fsck.collect_dirents(&manifest, &mut e, 0);
            e.into_iter()
                .filter(|(_, n)| n != "." && n != "..")
                .map(|(child, name)| (name, child))
                .collect()
        };
        ents.retain(|(name, child)| !m.remove.contains(&(*child, name.clone())));
        for a in &m.add { ents.push(a.clone()); }

        let (new_hash, npr) = publish_dir(io, f, &ents, &mut free, r.hash_alg,
            new.pack_registry_root)?;
        new.pack_registry_root = npr;
        free_dirty = true;
        if m.from_empty {
            // Create the directory inode pointing at the freshly-published
            // manifest (nlink 2; the parent's new dirent is its one link).
            let rec = make_dir_inode(dir_ino, &new_hash);
            new.inode_root = put_inode(io, new.inode_root, &dir_ino.to_be_bytes(), &rec)?;
        } else {
            new.inode_root = set_inode_manifest(io, new.inode_root, dir_ino, &new_hash, fsck)?;
        }
        applied += (m.remove.len() + m.add.len()) as u32;
        if verbose {
            eprintln!("  dir {dir_ino}: republished ({} removed, {} added){}",
                m.remove.len(), m.add.len(), if m.from_empty { " [minted]" } else { "" });
        }
    }

    // ── Tier A.1: nlink correction (inode-record rewrites) ──
    // ── Tier B.3: inode whose own manifest blob is gone ──
    // Nothing references the lost blob any more, so the entries/content it
    // described are unrecoverable. fsck_ffs's equivalent is to clear the
    // object rather than leave an unusable one: reset the inode to empty
    // (null manifest, size 0) so it mounts and can be removed/rewritten.
    // Children that the lost directory listing referenced become orphans and
    // the iterate-to-fixpoint pass relinks them into lost+found.
    if !fsck.dangling_manifests.is_empty() {
        unsafe {
            let t = tessera_btree_open(io, new.inode_root, TESSERA_BTREE_KIND_INODE,
                4, TESSERA_INODE_RECORD_SIZE);
            if t.is_null() { return Err("open inode tree for dangling-manifest repair".into()); }
            let mut root = new.inode_root;
            for (ino, _mode) in &fsck.dangling_manifests {
                let (key, raw) = match fsck.inode_raw.get(ino) {
                    Some(x) => x,
                    None => continue,
                };
                let mut patched = raw.clone();
                patched[56..64].copy_from_slice(&0u64.to_le_bytes());   // size
                patched[72..104].copy_from_slice(&[0u8; 32]);           // manifest_hash
                if applied >= budget { *truncated = true; break; }
                if tessera_btree_put(t, key.as_ptr(), patched.as_ptr(), &mut root) != 0 {
                    /* Out of reserve. Keep what we have (#96) — the old
                     * behaviour discarded every repair computed so far. */
                    eprintln!("tessera-fsck: reserve exhausted at inode {ino} \
(dangling-manifest repair) — committing {applied} completed repair(s); re-run to continue");
                    *truncated = true;
                    break;
                }
                applied += 1;
            }
            tessera_btree_close(t);
            new.inode_root = root;
        }
        if verbose {
            eprintln!("  reset {} inode(s) with a missing manifest blob to empty",
                fsck.dangling_manifests.len());
        }
        /*
         * Resetting a DIRECTORY empties it: every name it held is gone, and
         * its children become orphans that the next pass relinks into
         * lost+found under synthetic inode_N names. The volume comes out
         * CLEAN, but the SYSTEM can be broken — emptying /var cost a dev root
         * its /var/run, after which login, sshd, devd and cron all failed with
         * "No such file or directory" on a filesystem that was perfectly
         * consistent and writable. Say so loudly; a CLEAN result must not be
         * read as "the system still works".
         */
        let dirs: Vec<u32> = fsck.dangling_manifests.iter()
            .filter(|(_, mode)| mode & S_IFMT == S_IFDIR)
            .map(|(ino, _)| *ino).collect();
        if !dirs.is_empty() {
            eprintln!("tessera-fsck: WARNING — {} of those were DIRECTORIES \
                (inode(s) {}); their entries are UNRECOVERABLE and have been \
                discarded. Any child that survived is relinked into \
                lost+found under an inode_N name. If one of these was a \
                system directory (/var, /etc, /usr/lib ...) the volume is now \
                CLEAN but the installed system may not boot or run correctly \
                — check those paths and restore from backup or mtree before \
                relying on it.",
                dirs.len(),
                dirs.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(", "));
        }
    }

    if !fsck.nlink_fixes.is_empty() {
        unsafe {
            let t = tessera_btree_open(io, new.inode_root, TESSERA_BTREE_KIND_INODE,
                4, TESSERA_INODE_RECORD_SIZE);
            if t.is_null() { return Err("open inode tree for repair".into()); }
            let mut root = new.inode_root;
            for (key, val) in &fsck.nlink_fixes {
                if applied >= budget { *truncated = true; break; }
                if tessera_btree_put(t, key.as_ptr(), val.as_ptr(), &mut root) != 0 {
                    eprintln!("tessera-fsck: reserve exhausted during nlink repair \
— committing {applied} completed repair(s); re-run to continue");
                    *truncated = true;
                    break;
                }
                applied += 1;
            }
            tessera_btree_close(t);
            new.inode_root = root;
        }
        if verbose { eprintln!("  nlink: rewrote {} inode record(s)", fsck.nlink_fixes.len()); }
    }

    // ── Tier A.2: quota used_bytes correction ──
    if !fsck.quota_fixes.is_empty() {
        unsafe {
            let t = tessera_btree_open(io, new.quota_tree_root, TESSERA_BTREE_KIND_QUOTA,
                8, 128);
            if t.is_null() { return Err("open quota tree for repair".into()); }
            let mut root = new.quota_tree_root;
            for (key, val) in &fsck.quota_fixes {
                if applied >= budget { *truncated = true; break; }
                if tessera_btree_put(t, key.as_ptr(), val.as_ptr(), &mut root) != 0 {
                    eprintln!("tessera-fsck: reserve exhausted during quota repair \
— committing {applied} completed repair(s); re-run to continue");
                    *truncated = true;
                    break;
                }
                applied += 1;
            }
            tessera_btree_close(t);
            new.quota_tree_root = root;
        }
        if verbose { eprintln!("  quota: rewrote {} domain record(s)", fsck.quota_fixes.len()); }
    }

    // ── Tier B.4: retire damaged snapshots (task #85) ──
    // A snapshot whose tree references blobs that are in no pack cannot be
    // read back, and — unlike the live tree — there is nothing to patch: it
    // is immutable history. Deleting the record is the whole repair, and it
    // is a cheap and legitimate one (a snapshot is an automatic checkpoint,
    // not user data). It also un-anchors whatever packs only that snapshot
    // held, so the next GC can reclaim them.
    //
    // Deliberately AFTER the inode/dirent repairs and BEFORE the free-tree
    // rebuild: retiring changes only the snapshots tree, but the rebuild
    // below must be the last thing that observes allocation state.
    if !fsck.damaged_snapshots.is_empty() {
        if r.snapshots_root == 0 {
            return Err("damaged snapshots reported but snapshots_root is 0".into());
        }
        unsafe {
            let t = tessera_btree_open(io, new.snapshots_root,
                TESSERA_BTREE_KIND_SNAPSHOT, 8, TESSERA_SNAPSHOT_RECORD_SIZE);
            if t.is_null() { return Err("open snapshots tree for retire".into()); }
            let mut root = new.snapshots_root;
            for gen in &fsck.damaged_snapshots {
                // Key is the 8-byte BIG-endian generation (see format.h).
                let key = gen.to_be_bytes();
                if tessera_btree_delete(t, key.as_ptr(), &mut root) != 0 {
                    tessera_btree_close(t);
                    return Err(format!("btree_delete(snapshot {gen}) during retire"));
                }
                applied += 1;
                println!("  retired damaged snapshot generation {gen}");
            }
            tessera_btree_close(t);
            new.snapshots_root = root;
        }
        if verbose {
            eprintln!("  snapshots: retired {} damaged snapshot(s)",
                fsck.damaged_snapshots.len());
        }
    }

    // ── Tier A.3: rebuild the free-extent tree from the authoritative free
    //    set (Pass-D complement minus any sectors Tier-B just carved for new
    //    packs). Fixes leaked space, double-state, and free overlap in one
    //    shot, and keeps new packs out of the free tree. ──
    if free_dirty {
        let runs: Vec<(u64, u64)> = free.iter().copied().filter(|(_, l)| *l > 0).collect();
        unsafe {
            let ea = tessera_extent_open(io, 0); // fresh, empty
            if ea.is_null() { return Err("open extent allocator for repair".into()); }
            for (s, l) in &runs {
                if tessera_extent_free(ea, *s, *l) != 0 {
                    tessera_extent_close(ea);
                    return Err(format!("extent_free([{s}..{}]) during free-tree rebuild", s + l));
                }
            }
            let mut nr = 0u64;
            let rc = tessera_extent_flush(ea, &mut nr);
            tessera_extent_close(ea);
            if rc != 0 {
                /*
                 * BEST EFFORT. The rebuild writes a WHOLE new free-extent
                 * btree out of the metadata-reserve bump allocator and never
                 * reuses the sectors the old tree occupied, so its cost scales
                 * with the number of free runs and is capped by whatever
                 * reserve is left. On a large, churned volume that runs out
                 * (rc=-5 TESSERA_ENOSPC observed on a 25 GiB / 63917-pack dev
                 * root at generation 4360) — and because apply_repairs is
                 * atomic, failing here discarded EVERY other repair with it,
                 * making the volume unrepairable even though all its actual
                 * problems had repair actions that had already succeeded.
                 *
                 * Not rebuilding the free tree only leaves space unaccounted —
                 * the same class fsck already reports as a NOTE ("neither
                 * allocated nor free"), not as an inconsistency. Losing space
                 * accounting is strictly better than losing the repair, so
                 * keep the existing tree and commit everything else.
                 */
                eprintln!("tessera-fsck: WARNING — free-tree rebuild skipped \
                    (extent_flush rc={rc}); space accounting left as-is, all \
                    other repairs still applied");
                if rc == -5 {
                    eprintln!("tessera-fsck:   cause is metadata-reserve \
                        exhaustion, not a full data zone — run tessera-repack \
                        to reclaim reserve, then re-run --repair to rebuild \
                        the free tree");
                }
            } else {
                new.free_extent_root = nr;
                applied += 1;
                if verbose {
                    eprintln!("  free-tree: rebuilt from {} free run(s)",
                        runs.len());
                }
            }
        }
    }

    Ok((applied, new))
}

/// First-fit allocate `n` contiguous sectors from the running free set,
/// shrinking the chosen run. Zero-length remnants are filtered at flush.
fn alloc_extent(free: &mut [(u64, u64)], n: u64) -> Result<u64, String> {
    for run in free.iter_mut() {
        if run.1 >= n {
            let start = run.0;
            run.0 += n;
            run.1 -= n;
            return Ok(start);
        }
    }
    Err(format!("no free extent of {n} sector(s) for a repair pack"))
}

/// A 144-byte DIRECTORY inode record for a freshly-minted directory
/// (lost+found): mode drwxr-xr-x, nlink 2, gen 1, pointing at `hash`.
fn make_dir_inode(ino: u32, hash: &Hash) -> Vec<u8> {
    let mut r = vec![0u8; TESSERA_INODE_RECORD_SIZE as usize];
    r[0..4].copy_from_slice(&ino.to_le_bytes());       // inode_no
    r[4..8].copy_from_slice(&1u32.to_le_bytes());      // gen
    r[8..12].copy_from_slice(&0o040755u32.to_le_bytes()); // mode S_IFDIR|0755
    r[64..68].copy_from_slice(&2u32.to_le_bytes());    // nlink
    r[72..104].copy_from_slice(hash);                  // manifest_hash
    r
}

/// btree_put a raw inode record; returns the new inode-tree root.
fn put_inode(io: &tessera_block_io_t, inode_root: u64, key: &[u8], val: &[u8])
    -> Result<u64, String>
{
    unsafe {
        let t = tessera_btree_open(io, inode_root, TESSERA_BTREE_KIND_INODE, 4,
            TESSERA_INODE_RECORD_SIZE);
        if t.is_null() { return Err("open inode tree".into()); }
        let mut root = inode_root;
        let rc = tessera_btree_put(t, key.as_ptr(), val.as_ptr(), &mut root);
        tessera_btree_close(t);
        if rc != 0 { return Err("btree_put(inode)".into()); }
        Ok(root)
    }
}

/// Rewrite an existing inode's manifest_hash (offset 72). Returns new root.
fn set_inode_manifest(io: &tessera_block_io_t, inode_root: u64, ino: u32, hash: &Hash,
    fsck: &Fsck) -> Result<u64, String>
{
    let (key, raw) = fsck.inode_raw.get(&ino)
        .ok_or_else(|| format!("inode {ino} record missing for manifest update"))?;
    let mut rec = raw.clone();
    rec[72..104].copy_from_slice(hash);
    put_inode(io, inode_root, key, &rec)
}

/// Build a flat DIRECTORY manifest from `entries`, publish it as a
/// single-blob pack (content-addressed pack_id = hash[0..16], SEALED,
/// pack_kind 0 — matching the kmod's publish_manifest_to_disk), allocate
/// pack-zone space, write it, and register it. Returns (manifest hash, new
/// pack-registry root). Mirrors tessera_fs_publish_manifest_to_disk.
fn publish_dir(io: &tessera_block_io_t, f: &std::fs::File, entries: &[(String, u64)],
    free: &mut [(u64, u64)], hash_alg: u32, pack_root: u64) -> Result<(Hash, u64), String>
{
    unsafe {
        // 1. build the manifest
        let mb = tessera_manifest_begin(TESSERA_MFT_DIRECTORY);
        if mb.is_null() { return Err("manifest_begin(DIRECTORY)".into()); }
        tessera_manifest_set_hash_alg(mb, hash_alg);
        for (name, child) in entries {
            if tessera_manifest_add_dirent(mb, *child,
                name.as_ptr() as *const core::ffi::c_char, name.len()) != 0 {
                tessera_manifest_free(mb);
                return Err(format!("add_dirent('{name}')"));
            }
        }
        // finalize into an escalating buffer (dir manifests are small; a huge
        // flat directory would need DIRECTORY_2L — reported, not silently lost)
        let mut mbytes = Vec::new();
        let mut mlen = 0usize;
        let mut hash = [0u8; 32];
        let mut ok = false;
        for cap in [64 * 1024usize, 1 << 20, 16 << 20] {
            mbytes = vec![0u8; cap];
            let rc = tessera_manifest_finalize(mb, mbytes.as_mut_ptr(), cap,
                &mut mlen, hash.as_mut_ptr());
            if rc == 0 { ok = true; break; }
        }
        tessera_manifest_free(mb);
        if !ok {
            return Err("directory too large to re-emit as a flat manifest \
                (needs DIRECTORY_2L — not yet supported by repair)".into());
        }

        // 2. pack it (content-addressed pack_id = first 16 bytes of the hash)
        let pack_id = &hash[0..16];
        let pb = tessera_pack_begin(0, pack_id.as_ptr(), 0);
        if pb.is_null() { return Err("pack_begin".into()); }
        if tessera_pack_add_blob(pb, hash.as_ptr(), mbytes.as_ptr(), mlen as u32,
            TESSERA_BLOB_FLAG_MANIFEST) != 0 {
            tessera_pack_free(pb);
            return Err("pack_add_blob".into());
        }
        let mut psz = 0usize;
        tessera_pack_finalize(pb, std::ptr::null_mut(), 0, &mut psz);
        if psz == 0 || psz % SECTOR_SIZE as usize != 0 {
            tessera_pack_free(pb);
            return Err(format!("pack_finalize probe gave bad size {psz}"));
        }
        let mut pbuf = vec![0u8; psz];
        let rc = tessera_pack_finalize(pb, pbuf.as_mut_ptr(), psz, &mut psz);
        tessera_pack_free(pb);
        if rc != 0 { return Err("pack_finalize fill".into()); }

        // 3. allocate pack-zone space + write the pack
        let n_sectors = psz as u64 / SECTOR_SIZE;
        let start = alloc_extent(free, n_sectors)?;
        f.write_at(&pbuf, start * SECTOR_SIZE)
            .map_err(|e| format!("write pack at sector {start}: {e}"))?;

        // 4. register the pack (raw 64-byte tessera_registry_entry_t, LE;
        //    same field layout fsck reads back in Pass A)
        let mut re = [0u8; TESSERA_REGISTRY_ENTRY_SIZE as usize];
        re[0..16].copy_from_slice(pack_id);
        re[16..24].copy_from_slice(&start.to_le_bytes());       // start_sector
        re[24..32].copy_from_slice(&n_sectors.to_le_bytes());   // length_sectors
        re[32..36].copy_from_slice(&1u32.to_le_bytes());        // blob_count
        re[36..40].copy_from_slice(&0u32.to_le_bytes());        // pack_kind
        re[40..48].copy_from_slice(&(psz as u64).to_le_bytes()); // total_bytes
        // create_time (48..56) = 0
        re[56..60].copy_from_slice(&1u32.to_le_bytes());        // reachable_blobs
        re[60..64].copy_from_slice(&TESSERA_REGISTRY_FLAG_SEALED.to_le_bytes()); // flags

        let t = tessera_btree_open(io, pack_root, TESSERA_BTREE_KIND_PACK_REG, 16,
            TESSERA_REGISTRY_ENTRY_SIZE);
        if t.is_null() { return Err("open pack registry for repair".into()); }
        let mut npr = pack_root;
        let rc = tessera_btree_put(t, pack_id.as_ptr(), re.as_ptr(), &mut npr);
        tessera_btree_close(t);
        if rc != 0 { return Err("btree_put(pack registry)".into()); }
        Ok((hash, npr))
    }
}

fn main() -> ExitCode {
    let args: Vec<_> = std::env::args().collect();
    let mut path = None;
    let mut verbose = false;
    let mut repair = false;
    for a in &args[1..] {
        match a.as_str() {
            "-v" | "--verbose" => verbose = true,
            // --repair / -y: apply safe repairs in place, then re-verify.
            // Opens the device O_SYNC read-write. -n forces detect-only.
            "-y" | "--repair" => repair = true,
            "-n" | "--dry-run" => repair = false,
            s if !s.starts_with('-') => path = Some(s.to_string()),
            _ => {}
        }
    }
    let path = match path {
        Some(p) => p,
        None => {
            eprintln!("usage: tessera-fsck [-v] [-y|--repair | -n] PATH");
            eprintln!("  default: read-only check (exit 1 if problems found)");
            eprintln!("  -y, --repair: apply repairs in place, then re-verify");
            return ExitCode::from(2);
        }
    };
    let result = if repair {
        run_to_fixpoint(&path, verbose)
    } else {
        run(&path, verbose, false, u32::MAX).map(|(exit, _)| exit)
    };
    match result {
        Ok(0) => ExitCode::SUCCESS,
        Ok(_) => ExitCode::FAILURE,
        Err(e) => { eprintln!("tessera-fsck: {e}"); ExitCode::from(2) }
    }
}
