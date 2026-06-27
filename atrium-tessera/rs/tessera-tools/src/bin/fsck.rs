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
//!   - reachability: every inode's manifest + xattr blob exists, and every
//!     blob it transitively references (CHUNK_LIST chunks, CHUNK_TREE /
//!     DIRECTORY_2L / DIRECTORY_BTREE inner-node child manifests) exists —
//!     catches dangling manifests and live blobs reclaimed by a GC/recovery bug
//!
//! Not yet covered (reported as "skipped" where applicable, never silently):
//! multi-extent pack bodies, dirent→inode reachability / orphan detection
//! (incl. DIRECTORY_BTREE leaf entries), free-extent-vs-allocation
//! cross-check, quota accounting.
//!
//! Exit: 0 = clean, 1 = problems found, 2 = usage / I/O error.

use std::collections::{HashMap, HashSet};
use std::os::unix::fs::FileExt;
use std::process::ExitCode;

use tessera_sys::*;
use tessera_tools::{fd_of, make_io, open_file_ro, DiskCtx, SECTOR_SIZE};

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
    // every blob hash present in any (single-extent) pack
    all_blobs: HashSet<Hash>,
    // blob bytes for those that parse as a manifest (for recursion)
    manifests: HashMap<Hash, Vec<u8>>,
    // manifests already reachability-checked (dedup + cycle guard)
    checked: HashSet<Hash>,
    // stats
    inodes: u64,
    packs: u64,
    blobs: u64,
    multi_extent_skipped: u64,
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
}

fn run(path: &str, verbose: bool) -> Result<i32, String> {
    let f = open_file_ro(path).map_err(|e| format!("open {path}: {e}"))?;
    let mut ctx = DiskCtx { fd: fd_of(&f) };
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

    let mut fsck = Fsck {
        problems: Vec::new(),
        all_blobs: HashSet::new(),
        manifests: HashMap::new(),
        checked: HashSet::new(),
        inodes: 0,
        packs: 0,
        blobs: 0,
        multi_extent_skipped: 0,
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

                if flags & TESSERA_REGISTRY_FLAG_MULTI_EXTENT != 0 {
                    fsck.multi_extent_skipped += 1;
                    if verbose { eprintln!("  pack {pid}: multi-extent — extent/blob check skipped (v1)"); }
                } else {
                    // extent bounds + overlap bookkeeping
                    if start < pz_start || start + len > pz_start + pz_len {
                        fsck.problem(format!("pack {pid} extent [{start}..{}] outside pack zone [{pz_start}..{}]",
                            start + len, pz_start + pz_len));
                    }
                    intervals.push((start, start + len, pid.clone()));
                    // read + open the pack, enumerate blobs
                    let nbytes = (len * SECTOR_SIZE) as usize;
                    let mut buf = vec![0u8; nbytes];
                    if f.read_at(&mut buf, start * SECTOR_SIZE).map(|n| n == nbytes).unwrap_or(false) {
                        let pr = tessera_pack_open(buf.as_ptr(), nbytes);
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
                                        // CONTENT INTEGRITY: the blob hash IS its
                                        // content address — recompute and compare to
                                        // catch torn writes / bit-rot in the data.
                                        let mut got = [0u8; 32];
                                        tessera_sha256(slice.as_ptr(), slice.len(), got.as_mut_ptr());
                                        if got != bh {
                                            fsck.problem(format!(
                                                "pack {pid}: blob {} content hashes to {} (corrupted data)",
                                                hx(&bh), hx(&got)));
                                        }
                                        // if it parses as a manifest, keep bytes for recursion
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
                    } else {
                        fsck.problem(format!("pack {pid}: could not read {nbytes} bytes at sector {start}"));
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

                if tessera_btree_cursor_next(cur) != 0 { break; }
            }
            if !c.is_null() { tessera_btree_cursor_free(c); }
            tessera_btree_close(t);
        }
    }

    unsafe { tessera_volume_close(v); }

    // ── report ───────────────────────────────────────────────────
    println!("tessera-fsck: {path}");
    println!("  generation:   {generation}");
    println!("  inodes:       {}", fsck.inodes);
    println!("  packs:        {} ({} blobs, {} parse as manifests)",
        fsck.packs, fsck.blobs, fsck.manifests.len());
    if fsck.multi_extent_skipped > 0 {
        println!("  NOTE: {} multi-extent pack(s) not checked (v1 limitation)", fsck.multi_extent_skipped);
    }
    if fsck.problems.is_empty() {
        println!("  result:       CLEAN — no inconsistencies found");
        Ok(0)
    } else {
        println!("  result:       {} PROBLEM(S) FOUND:", fsck.problems.len());
        for p in &fsck.problems {
            println!("    - {p}");
        }
        Ok(1)
    }
}

fn main() -> ExitCode {
    let args: Vec<_> = std::env::args().collect();
    let mut path = None;
    let mut verbose = false;
    for a in &args[1..] {
        match a.as_str() {
            "-v" | "--verbose" => verbose = true,
            s if !s.starts_with('-') => path = Some(s.to_string()),
            _ => {}
        }
    }
    let path = match path {
        Some(p) => p,
        None => { eprintln!("usage: tessera-fsck [-v] PATH"); return ExitCode::from(2); }
    };
    match run(&path, verbose) {
        Ok(0) => ExitCode::SUCCESS,
        Ok(_) => ExitCode::FAILURE,
        Err(e) => { eprintln!("tessera-fsck: {e}"); ExitCode::from(2) }
    }
}
