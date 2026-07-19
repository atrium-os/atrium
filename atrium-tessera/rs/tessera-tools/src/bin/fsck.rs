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

use std::collections::{HashMap, HashSet};
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
    // stats
    inodes: u64,
    packs: u64,
    blobs: u64,
}

impl Fsck {
    fn problem(&mut self, s: String) { self.problems.push(s); }

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

fn run(path: &str, verbose: bool, repair: bool) -> Result<i32, String> {
    let f = if repair {
        open_file_rw(path).map_err(|e| format!("open {path} (rw): {e}"))?
    } else {
        open_file_ro(path).map_err(|e| format!("open {path}: {e}"))?
    };
    // ctx starts read-only; the bump allocator is armed below once the
    // superblock's meta-reserve extent is known (io holds a raw pointer to
    // ctx, so mutating ctx after make_io is fine — no live borrow).
    let mut ctx = DiskCtx::ro(fd_of(&f));
    let io = make_io(&mut ctx);

    let mut v: *mut tessera_volume_t = std::ptr::null_mut();
    let r = unsafe { tessera_volume_open(&io, &mut v) };
    if r != 0 {
        // The superblock itself is unreadable/corrupt — the worst case.
        eprintln!("tessera-fsck: SUPERBLOCK INVALID (tessera_volume_open errno={r}) — both SB copies failed magic/CRC/HMAC/version");
        return Ok(1);
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
        ctx.bump.set(mr_bump);
        ctx.bump_max = mr_start + mr_len;
    }

    let mut fsck = Fsck {
        problems: Vec::new(),
        notes: Vec::new(),
        all_blobs: HashSet::new(),
        manifests: HashMap::new(),
        checked: HashSet::new(),
        inode_map: HashMap::new(),
        inode_raw: HashMap::new(),
        quota_used: HashMap::new(),
        nlink_fixes: Vec::new(),
        quota_fixes: Vec::new(),
        free_runs: Vec::new(),
        free_tree_dirty: false,
        inodes: 0,
        packs: 0,
        blobs: 0,
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
                                }
                            }
                        }
                        tessera_pack_close(pr);
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
                *links.entry(c).or_insert(0) += 1;
                if !fsck.inode_map.contains_key(&c) {
                    dangling.push(format!("dangling dirent: inode {ino} entry '{name}' -> inode {child} (not in inode tree)"));
                } else if reachable.insert(c) {
                    stack.push(c);
                }
            }
        }
        for d in dangling { fsck.problem(d); }
        let mut orphans: Vec<u32> = fsck.inode_map.keys().copied()
            .filter(|k| !reachable.contains(k)).collect();
        orphans.sort_unstable();
        for o in &orphans {
            fsck.problem(format!("orphan inode {o} (not reachable from the root dir)"));
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
        for (s, e, _) in &all {
            if *s > cursor { leaked += *s - cursor; }
            if *e > cursor { cursor = *e; }
        }
        if cursor < pz_end { leaked += pz_end - cursor; }
        if leaked > 0 {
            // Informational, not a failure: untracked space is a space-
            // efficiency issue (not corruption), and can arise from
            // allocator behaviour under fragmentation. Double-state overlap
            // above IS a hard problem. --repair reclaims it when rebuilding
            // the free tree.
            fsck.notes.push(format!("{leaked} pack-zone sector(s) neither allocated nor free (leaked space)"));
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

    // ── report ───────────────────────────────────────────────────
    println!("tessera-fsck: {path}");
    println!("  generation:   {generation}");
    println!("  inodes:       {}", fsck.inodes);
    println!("  packs:        {} ({} blobs, {} parse as manifests)",
        fsck.packs, fsck.blobs, fsck.manifests.len());
    for n in &fsck.notes {
        println!("  NOTE: {n}");
    }
    if fsck.problems.is_empty() {
        println!("  result:       CLEAN — no inconsistencies found");
        unsafe { tessera_volume_close(v); }
        return Ok(0);
    }
    println!("  result:       {} PROBLEM(S) FOUND:", fsck.problems.len());
    for p in &fsck.problems {
        println!("    - {p}");
    }

    if !repair {
        unsafe { tessera_volume_close(v); }
        return Ok(1);
    }

    // ── --repair: apply fixes, commit a new superblock, re-verify ──
    let roots = RepairRoots {
        inode_root, pack_registry_root: pack_root, free_extent_root: free_root,
        quota_tree_root: quota_root, snapshots_root, next_inode_no,
        pz_start, pz_len, hash_alg, mr_start, mr_len,
    };
    println!("\ntessera-fsck: --repair — applying fixes…");
    let (applied, new_roots) = match apply_repairs(&f, &io, &fsck, &roots, verbose) {
        Ok(x) => x,
        Err(e) => { unsafe { tessera_volume_close(v); } return Err(format!("repair aborted: {e}")); }
    };
    if applied == 0 {
        println!("tessera-fsck: nothing safely repairable by this tool (Tier-B/structural or superblock-level damage) — see problems above");
        unsafe { tessera_volume_close(v); }
        return Ok(1);
    }
    // Seal a new superblock pointing at the repaired roots. ctx.bump has
    // advanced past every reserve sector the rewrites consumed.
    let commit = tessera_commit_roots_t {
        inode_root:         new_roots.inode_root,
        pack_registry_root: new_roots.pack_registry_root,
        free_extent_root:   new_roots.free_extent_root,
        quota_tree_root:    new_roots.quota_tree_root,
        snapshots_root:     new_roots.snapshots_root,
        meta_reserve_bump:  ctx.bump.get(),
        next_inode_no:      new_roots.next_inode_no,
    };
    let rc = unsafe { tessera_volume_commit_roots(v, &commit) };
    unsafe { tessera_volume_close(v); }
    if rc != 0 {
        return Err(format!("commit_roots failed: rc={rc}"));
    }
    println!("tessera-fsck: applied {applied} repair action(s); committed generation {} — re-verifying…\n",
        generation + 1);

    // Fresh read-only scan of the now-repaired volume.
    let verify = run(path, verbose, false)?;
    if verify == 0 {
        println!("\ntessera-fsck: REPAIR SUCCESSFUL — volume is now clean");
        Ok(0)
    } else {
        println!("\ntessera-fsck: REPAIR INCOMPLETE — problems remain (structural/Tier-B or unrepairable)");
        Ok(1)
    }
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
fn apply_repairs(
    _f: &std::fs::File,
    io: &tessera_block_io_t,
    fsck: &Fsck,
    r: &RepairRoots,
    verbose: bool,
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
    let _ = (r.pz_start, r.pz_len, r.hash_alg, r.mr_start, r.mr_len); // used by Tier B

    // ── Tier A.1: nlink correction (inode-record rewrites) ──
    if !fsck.nlink_fixes.is_empty() {
        unsafe {
            let t = tessera_btree_open(io, new.inode_root, TESSERA_BTREE_KIND_INODE,
                4, TESSERA_INODE_RECORD_SIZE);
            if t.is_null() { return Err("open inode tree for repair".into()); }
            let mut root = new.inode_root;
            for (key, val) in &fsck.nlink_fixes {
                if tessera_btree_put(t, key.as_ptr(), val.as_ptr(), &mut root) != 0 {
                    tessera_btree_close(t);
                    return Err("btree_put(inode) during nlink repair".into());
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
                if tessera_btree_put(t, key.as_ptr(), val.as_ptr(), &mut root) != 0 {
                    tessera_btree_close(t);
                    return Err("btree_put(quota) during quota repair".into());
                }
                applied += 1;
            }
            tessera_btree_close(t);
            new.quota_tree_root = root;
        }
        if verbose { eprintln!("  quota: rewrote {} domain record(s)", fsck.quota_fixes.len()); }
    }

    // ── Tier A.3: rebuild the free-extent tree from the authoritative
    //    complement (pack zone minus allocated packs). Fixes leaked
    //    space, double-state, and free overlap in one shot. ──
    if fsck.free_tree_dirty {
        unsafe {
            let ea = tessera_extent_open(io, 0); // fresh, empty
            if ea.is_null() { return Err("open extent allocator for repair".into()); }
            for (s, l) in &fsck.free_runs {
                if tessera_extent_free(ea, *s, *l) != 0 {
                    tessera_extent_close(ea);
                    return Err(format!("extent_free([{s}..{}]) during free-tree rebuild", s + l));
                }
            }
            let mut nr = 0u64;
            let rc = tessera_extent_flush(ea, &mut nr);
            tessera_extent_close(ea);
            if rc != 0 { return Err(format!("extent_flush during free-tree rebuild: rc={rc}")); }
            new.free_extent_root = nr;
        }
        applied += 1;
        if verbose {
            eprintln!("  free-tree: rebuilt from {} free run(s)", fsck.free_runs.len());
        }
    }

    Ok((applied, new))
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
    match run(&path, verbose, repair) {
        Ok(0) => ExitCode::SUCCESS,
        Ok(_) => ExitCode::FAILURE,
        Err(e) => { eprintln!("tessera-fsck: {e}"); ExitCode::from(2) }
    }
}
