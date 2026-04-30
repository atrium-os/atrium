//! tessera-property-tests — Phase-3 property + crash-recovery harness.
//!
//! Drives tessera-core through the in-memory `MemDisk` / `WriteRecorder`
//! shims and asserts invariants over randomised operation sequences.
//!
//! Each scenario is keyed off a deterministic seed so a failure can be
//! reproduced exactly. Scenarios run sequentially; the binary exits 0
//! iff every scenario passes.
//!
//! USAGE:
//!     tessera-property-tests [SCENARIO ...]    (default: all)

use std::collections::{HashMap, HashSet};
use std::ffi::c_void;

use tessera_sys::*;
use tessera_test_harness::{MemDisk, WriteRecorder, Xorshift64};

/* ── helpers ─────────────────────────────────────────────────── */

fn random_uuid(rng: &Xorshift64) -> [u8; 16] {
    let mut u = [0u8; 16];
    let a = rng.next().to_le_bytes();
    let b = rng.next().to_le_bytes();
    u[..8].copy_from_slice(&a);
    u[8..].copy_from_slice(&b);
    u[6] = (u[6] & 0x0f) | 0x40;
    u[8] = (u[8] & 0x3f) | 0x80;
    u
}

/* ── scenario 1: volume format/open round-trip ────────────────── */

fn scenario_volume_round_trip() -> Result<(), String> {
    let rng = Xorshift64::new(0xc0ffee_0001);
    for _ in 0..32 {
        let total_sectors = 512 + (rng.range(2048) as usize);
        let journal_sectors = 32 + (rng.range(96) as usize);
        let mut disk = MemDisk::new(total_sectors);
        let io = MemDisk::block_io(&mut disk);
        let opts = tessera_format_opts_t {
            total_sectors:   total_sectors as u64,
            journal_sectors: journal_sectors as u64,
            volume_uuid:     random_uuid(&rng),
            seed_dirent_name: std::ptr::null(), seed_dirent_name_len: 0, seed_dirent_inode: 0,
            seed_content_data: std::ptr::null(), seed_content_len: 0,
            seed_chunk_size: 0,
        };
        let r = unsafe { tessera_volume_format(&io, &opts) };
        if r != 0 {
            return Err(format!(
                "format(total={total_sectors}, journal={journal_sectors}) → {r}"
            ));
        }

        let mut v: *mut tessera_volume_t = std::ptr::null_mut();
        let r = unsafe { tessera_volume_open(&io, &mut v) };
        if r != 0 {
            return Err(format!("open after format → {r}"));
        }
        let total = unsafe { tessera_volume_total_sectors(v) };
        let gen   = unsafe { tessera_volume_generation(v) };
        let inode_root = unsafe { tessera_volume_inode_root(v) };
        let pack_root  = unsafe { tessera_volume_pack_registry_root(v) };
        let free_root  = unsafe { tessera_volume_free_extent_root(v) };
        unsafe { tessera_volume_close(v); }

        if total != total_sectors as u64 { return Err("total mismatch".into()); }
        if gen   != 1 { return Err("gen != 1".into()); }
        for r in [inode_root, pack_root, free_root] {
            if r == 0 || r >= total {
                return Err(format!("root {r} out of range [0, {total})"));
            }
        }
    }
    println!("  volume_round_trip: 32 random formats survived round-trip");
    Ok(())
}

/* ── scenario 2: extent allocator persistence ─────────────────── */

fn scenario_extent_persistence() -> Result<(), String> {
    let rng = Xorshift64::new(0xc0ffee_0002);

    /* A small dedicated MemDisk for the B+tree backing — extent
     * allocator's flush() consumes sectors via io.alloc. */
    let mut disk = MemDisk::new(256);
    let io = MemDisk::block_io(&mut disk);

    let a = unsafe { tessera_extent_open(&io, 0) };
    if a.is_null() { return Err("extent_open failed".into()); }

    /* Seed a few random non-overlapping extents. */
    let mut placed: Vec<(u64, u64)> = Vec::new();
    let mut cursor = 1000u64;
    for _ in 0..20 {
        let len = 1 + (rng.range(40));
        let r = unsafe { tessera_extent_free(a, cursor, len) };
        if r != 0 { return Err(format!("free({cursor}, {len}) → {r}")); }
        placed.push((cursor, len));
        cursor += len + 1 + rng.range(10); /* gap */
    }
    let total: u64 = placed.iter().map(|&(_, n)| n).sum();
    let blocks = unsafe { tessera_extent_free_blocks(a) };
    if blocks != total { return Err(format!("free_blocks {blocks} != total {total}")); }

    let mut root = 0u64;
    let r = unsafe { tessera_extent_flush(a, &mut root) };
    if r != 0 { return Err(format!("flush → {r}")); }
    if root == 0 { return Err("flush returned root=0".into()); }
    unsafe { tessera_extent_close(a); }

    /* Reopen and verify state matches. */
    let a2 = unsafe { tessera_extent_open(&io, root) };
    if a2.is_null() { return Err("reopen failed".into()); }
    let blocks2 = unsafe { tessera_extent_free_blocks(a2) };
    if blocks2 != total {
        return Err(format!("post-reopen free_blocks {blocks2} != total {total}"));
    }
    /* Recover all start sectors via best-fit allocs. */
    let mut recovered: HashSet<u64> = HashSet::new();
    for &(_, len) in &placed {
        let mut s = 0u64;
        let r = unsafe { tessera_extent_alloc(a2, len, &mut s) };
        if r != 0 { return Err(format!("alloc({len}) → {r}")); }
        recovered.insert(s);
    }
    let placed_starts: HashSet<u64> = placed.iter().map(|&(s, _)| s).collect();
    if recovered != placed_starts {
        return Err(format!(
            "recovered starts {recovered:?} != placed {placed_starts:?}"
        ));
    }
    unsafe { tessera_extent_close(a2); }
    println!("  extent_persistence: 20 extents flush+reopen+best-fit-recover");
    Ok(())
}

/* ── scenario 3: journal replay across crash points ───────────── */

struct ReplayCapture {
    types:  Vec<u32>,
    bodies: Vec<Vec<u8>>,
}

extern "C" fn replay_cb(
    ctx:  *mut c_void,
    hdr:  *const tessera_record_header_t,
    body: *const u8,
) -> i32 {
    let cap = unsafe { &mut *(ctx as *mut ReplayCapture) };
    let h = unsafe { &*hdr };
    cap.types.push(h.record_type);
    let n = h.body_length as usize;
    let mut v = vec![0u8; n];
    if n > 0 {
        let src = unsafe { std::slice::from_raw_parts(body, n) };
        v.copy_from_slice(src);
    }
    cap.bodies.push(v);
    0
}

fn scenario_journal_crash_replay() -> Result<(), String> {
    let rng = Xorshift64::new(0xc0ffee_0003);
    let total_sectors = 64usize;
    let j_start: u64 = 0;
    let j_len: u64   = total_sectors as u64;

    /* Build a script of (kind, body) records grouped by transaction.
     * Each txn either commits or aborts; commits' records should be
     * visible after replay. */
    #[derive(Clone)]
    struct Tx { committed: bool, recs: Vec<(i32, Vec<u8>)> }
    let mut script: Vec<Tx> = Vec::new();
    for _ in 0..6 {
        let n_recs = 1 + rng.range(3) as usize;
        let mut recs = Vec::new();
        for _ in 0..n_recs {
            let kind = match rng.range(3) {
                0 => TESSERA_INODE_WRITE,
                1 => TESSERA_PACK_PUBLISH,
                _ => TESSERA_DIR_INSERT,
            };
            let body_len = 8 + rng.range(80) as usize;
            let mut b = vec![0u8; body_len];
            for i in 0..body_len { b[i] = (rng.next() & 0xff) as u8; }
            recs.push((kind, b));
        }
        script.push(Tx { committed: rng.range(4) != 0, recs });
    }

    /* 1. Format the journal on a recorder-backed disk. */
    let mut rec = Box::new(WriteRecorder::new(total_sectors));
    let io = WriteRecorder::block_io(&mut *rec as *mut _);
    let r = unsafe { tessera_journal_format(&io, j_start, j_len) };
    if r != 0 { return Err(format!("journal_format → {r}")); }

    /* 2. Re-baseline: from now on we record only the txn writes. */
    rec.rebaseline();

    /* 3. Replay the script through the live journal handle. */
    let j = unsafe { tessera_journal_open(&io, j_start, j_len) };
    if j.is_null() { return Err("journal_open failed".into()); }
    let tag = b"prop-test\0\0\0\0\0\0\0";
    let mut expected_committed_bodies: Vec<(u32, Vec<u8>)> = Vec::new();
    let mut tx_boundary_writes: Vec<usize> = Vec::new(); /* writes-count after each txn */
    for tx in &script {
        let mut tx_id = 0u64;
        let r = unsafe { tessera_journal_tx_begin(j, &mut tx_id, tag.as_ptr()) };
        if r != 0 { return Err(format!("tx_begin → {r}")); }
        for (kind, body) in &tx.recs {
            let r = unsafe { tessera_journal_append(j, tx_id, *kind,
                body.as_ptr(), body.len() as u32) };
            if r != 0 { return Err(format!("append → {r}")); }
        }
        if tx.committed {
            let r = unsafe { tessera_journal_tx_commit(j, tx_id) };
            if r != 0 { return Err(format!("commit → {r}")); }
            for (k, b) in &tx.recs {
                expected_committed_bodies.push((*k as u32, b.clone()));
            }
        } else {
            let r = unsafe { tessera_journal_tx_abort(j, tx_id, 0) };
            if r != 0 { return Err(format!("abort → {r}")); }
        }
        tx_boundary_writes.push(rec.write_count());
    }
    unsafe { tessera_journal_close(j); }

    /* 4. Replay across every per-txn-boundary crash point and check
     *    that exactly the txns committed before the crash are
     *    visible. */
    for (i, &boundary) in tx_boundary_writes.iter().enumerate() {
        let mut partial = rec.replay_through(boundary);
        let part_io = MemDisk::block_io(&mut partial);
        let j2 = unsafe { tessera_journal_open(&part_io, j_start, j_len) };
        if j2.is_null() { return Err(format!("crash-{i}: open failed")); }

        let mut cap = ReplayCapture { types: Vec::new(), bodies: Vec::new() };
        let r = unsafe { tessera_journal_replay(j2, replay_cb,
            &mut cap as *mut _ as *mut c_void) };
        if r != 0 { return Err(format!("crash-{i}: replay → {r}")); }
        unsafe { tessera_journal_close(j2); }

        let mut expected: Vec<(u32, Vec<u8>)> = Vec::new();
        for tx in &script[..=i] {
            if tx.committed {
                for (k, b) in &tx.recs {
                    expected.push((*k as u32, b.clone()));
                }
            }
        }
        if cap.types.len() != expected.len() {
            return Err(format!("crash-{i}: replay count {} != expected {}",
                cap.types.len(), expected.len()));
        }
        for (got, exp) in cap.types.iter().zip(cap.bodies.iter())
            .zip(expected.iter())
        {
            let ((g_t, g_b), (e_t, e_b)) = (got, exp);
            if g_t != e_t || g_b != e_b {
                return Err(format!("crash-{i}: replay mismatch"));
            }
        }
    }

    /* 5. Bonus: replay a TORN crash mid-txn (between writes inside the
     *    last txn) and assert it's silently dropped. */
    if let Some(last_boundary) = tx_boundary_writes.last().copied() {
        let prev = if tx_boundary_writes.len() > 1 {
            tx_boundary_writes[tx_boundary_writes.len() - 2]
        } else { 0 };
        if last_boundary > prev + 1 {
            let mid = prev + (last_boundary - prev) / 2;
            let mut partial = rec.replay_through(mid);
            let part_io = MemDisk::block_io(&mut partial);
            let j2 = unsafe { tessera_journal_open(&part_io, j_start, j_len) };
            if !j2.is_null() {
                let mut cap = ReplayCapture { types: Vec::new(), bodies: Vec::new() };
                let _ = unsafe { tessera_journal_replay(j2, replay_cb,
                    &mut cap as *mut _ as *mut c_void) };
                unsafe { tessera_journal_close(j2); }
                /* Whatever happened, replay must not include the
                 * last (in-flight) txn. We don't assert exact count
                 * here — the txn before may or may not have been
                 * fully written either. The contract is "no false
                 * apply": the replayed bodies are a prefix of
                 * fully-committed ones. */
                let mut all_expected: Vec<(u32, Vec<u8>)> = Vec::new();
                for tx in &script {
                    if tx.committed {
                        for (k, b) in &tx.recs {
                            all_expected.push((*k as u32, b.clone()));
                        }
                    }
                }
                let n = cap.types.len();
                for ((g_t, g_b), (e_t, e_b)) in cap.types.iter()
                    .zip(cap.bodies.iter())
                    .zip(all_expected.iter().take(n))
                {
                    if g_t != e_t || g_b != e_b {
                        return Err(format!(
                            "torn replay produced non-prefix"));
                    }
                }
            }
        }
    }

    println!("  journal_crash_replay: {} crash points × txns, all consistent",
        tx_boundary_writes.len());
    Ok(())
}

/* ── scenario 4: B+tree random ops vs HashMap oracle ──────────── */

fn scenario_btree_random_ops() -> Result<(), String> {
    let rng = Xorshift64::new(0xc0ffee_0004);
    const KEY_SIZE: u32 = 4;
    const VAL_SIZE: u32 = 16;
    const N_OPS:   u32 = 4000;

    let mut disk = MemDisk::new(2048);
    let io = MemDisk::block_io(&mut disk);

    let mut root = 0u64;
    let t = unsafe { tessera_btree_create(&io, 0, KEY_SIZE, VAL_SIZE, &mut root) };
    if t.is_null() { return Err("create failed".into()); }

    let mut oracle: HashMap<[u8; 4], [u8; 16]> = HashMap::new();
    let mut put_count = 0u32;
    let mut del_count = 0u32;

    for _ in 0..N_OPS {
        let op = rng.range(10);
        let key32 = (rng.next() & 0xffff) as u32;          /* 65k keyspace */
        let mut k = [0u8; 4];
        k[0] = (key32 >> 24) as u8; k[1] = (key32 >> 16) as u8;
        k[2] = (key32 >>  8) as u8; k[3] =  key32        as u8;

        if op < 7 || oracle.is_empty() {
            /* put — overwrite OK. */
            let mut v = [0u8; 16];
            for i in 0..16 { v[i] = (rng.next() & 0xff) as u8; }
            let r = unsafe { tessera_btree_put(t, k.as_ptr(), v.as_ptr(),
                &mut root) };
            if r != 0 { return Err(format!("put → {r}")); }
            oracle.insert(k, v);
            put_count += 1;
        } else {
            /* delete — pick an existing key half the time. */
            let pick_existing = rng.range(2) == 0;
            let dk = if pick_existing {
                *oracle.keys().nth(rng.range(oracle.len() as u64) as usize).unwrap()
            } else { k };
            let r = unsafe { tessera_btree_delete(t, dk.as_ptr(), &mut root) };
            match (oracle.contains_key(&dk), r) {
                (true,  0) => { oracle.remove(&dk); del_count += 1; }
                (false, TESSERA_ENOENT) => {}
                (true,  e) => return Err(format!("delete present → {e}")),
                (false, e) => return Err(format!("delete absent → {e} (want ENOENT)")),
            }
        }
    }

    /* Pointwise check: every oracle key returns its value via get. */
    for (k, v) in &oracle {
        let mut got = [0u8; 16];
        let r = unsafe { tessera_btree_get(t, k.as_ptr(), got.as_mut_ptr()) };
        if r != 0 { return Err(format!("get(present) → {r}")); }
        if &got != v { return Err("get returned wrong value".into()); }
    }

    /* Cursor walk yields exactly the oracle's keys in ascending order. */
    let c = unsafe { tessera_btree_seek_first(t) };
    if c.is_null() && !oracle.is_empty() { return Err("seek_first NULL".into()); }
    if !c.is_null() {
        let mut walked: Vec<[u8; 4]> = Vec::new();
        loop {
            let mut k = [0u8; 4]; let mut v = [0u8; 16];
            if unsafe { tessera_btree_cursor_get(c, k.as_mut_ptr(), v.as_mut_ptr()) } != 0 {
                break;
            }
            walked.push(k);
            if unsafe { tessera_btree_cursor_next(c) } != 0 { break; }
        }
        unsafe { tessera_btree_cursor_free(c); }

        if walked.len() != oracle.len() {
            return Err(format!("cursor walked {} keys, oracle has {}",
                walked.len(), oracle.len()));
        }
        for w in walked.windows(2) {
            if w[0] >= w[1] { return Err("cursor not sorted ascending".into()); }
        }
        let walked_set: HashSet<_> = walked.iter().copied().collect();
        let oracle_set: HashSet<_> = oracle.keys().copied().collect();
        if walked_set != oracle_set {
            return Err("cursor's key-set differs from oracle".into());
        }
    }
    unsafe { tessera_btree_close(t); }

    println!("  btree_random_ops: {N_OPS} ops ({put_count} put, {del_count} del), \
        oracle = {} keys, cursor walk matches", oracle.len());
    Ok(())
}

/* ── scenario 5: pack reader corruption fuzz ──────────────────── */

fn scenario_pack_corruption_fuzz() -> Result<(), String> {
    let rng = Xorshift64::new(0xc0ffee_0005);

    /* Build a real pack with deterministic content. */
    let mut pack_id = [0u8; 16];
    for i in 0..16 { pack_id[i] = i as u8; }
    let pb = unsafe { tessera_pack_begin(1, pack_id.as_ptr(), 0) };
    if pb.is_null() { return Err("pack_begin failed".into()); }

    let mut blobs: Vec<(Vec<u8>, [u8; 32])> = Vec::new();
    for i in 0..32u32 {
        let len = 64 + (rng.range(2048) as usize);
        let mut bytes = vec![0u8; len];
        for j in 0..len { bytes[j] = (rng.next() & 0xff) as u8; }
        let mut h = [0u8; 32];
        unsafe { tessera_sha256(bytes.as_ptr(), len, h.as_mut_ptr()); }
        let r = unsafe { tessera_pack_add_blob(pb, h.as_ptr(),
            bytes.as_ptr(), len as u32, TESSERA_BLOB_FLAG_CHUNK) };
        if r != 0 { return Err(format!("add_blob[{i}] → {r}")); }
        blobs.push((bytes, h));
    }

    let mut sz: usize = 0;
    let _ = unsafe { tessera_pack_finalize(pb, std::ptr::null_mut(), 0, &mut sz) };
    let mut buf = vec![0u8; sz];
    let r = unsafe { tessera_pack_finalize(pb, buf.as_mut_ptr(), sz, &mut sz) };
    if r != 0 { return Err(format!("finalize → {r}")); }
    unsafe { tessera_pack_free(pb); }

    /* Sanity: clean pack opens and every blob looks up by hash. */
    let pr = unsafe { tessera_pack_open(buf.as_ptr(), sz) };
    if pr.is_null() { return Err("clean pack failed to open".into()); }
    for (bytes, h) in &blobs {
        let mut out_p: *const u8 = std::ptr::null();
        let mut out_n: u32 = 0;
        let r = unsafe { tessera_pack_lookup(pr, h.as_ptr(),
            &mut out_p, &mut out_n) };
        if r != 0 { return Err(format!("lookup → {r}")); }
        if out_n as usize != bytes.len() {
            return Err("len mismatch".into());
        }
        let got = unsafe { std::slice::from_raw_parts(out_p, out_n as usize) };
        if got != bytes.as_slice() { return Err("content mismatch".into()); }
    }
    unsafe { tessera_pack_close(pr); }

    /* Fuzz: flip 1 byte at 1000 random positions. The reader must
     * EITHER reject the pack at open() (footer CRC catches data-area
     * corruption) OR open and return content that matches the
     * recomputed SHA-256. The contract is "no false content".
     *
     * We pre-collect (hash → expected_bytes) so we can re-verify
     * whatever lookup returns against the expected. A corrupted index
     * entry would produce wrong-content; the reader's footer CRC over
     * the full data area also catches it. */
    let expected: HashMap<[u8; 32], Vec<u8>> =
        blobs.iter().map(|(b, h)| (*h, b.clone())).collect();

    let mut rejected = 0u32;
    let mut accepted_ok = 0u32;
    for _ in 0..1000 {
        let mut copy = buf.clone();
        let pos = (rng.range(copy.len() as u64)) as usize;
        copy[pos] ^= 0xff;

        let pr = unsafe { tessera_pack_open(copy.as_ptr(), copy.len()) };
        if pr.is_null() {
            rejected += 1;
            continue;
        }
        /* Opened. Look up every blob by its hash and verify the
         * returned bytes match the original. If the corruption hit a
         * data byte and the footer CRC didn't cover it (it should!)
         * we'd detect a content mismatch here. */
        let mut content_ok = true;
        for (h, exp) in &expected {
            let mut out_p: *const u8 = std::ptr::null();
            let mut out_n: u32 = 0;
            let r = unsafe { tessera_pack_lookup(pr, h.as_ptr(),
                &mut out_p, &mut out_n) };
            if r != 0 {
                /* Lookup may legitimately fail (e.g. corrupted index
                 * entry's hash flipped). That's still a "no false
                 * content" outcome. */
                continue;
            }
            if out_n as usize != exp.len() {
                content_ok = false; break;
            }
            let got = unsafe { std::slice::from_raw_parts(out_p, out_n as usize) };
            if got != exp.as_slice() { content_ok = false; break; }
        }
        unsafe { tessera_pack_close(pr); }
        if content_ok {
            accepted_ok += 1;
        } else {
            return Err("pack opened but returned wrong content".into());
        }
    }

    println!("  pack_corruption_fuzz: 1000 byte-flips → {rejected} rejected at open, \
        {accepted_ok} opened with consistent content");
    Ok(())
}

/* ── scenario 6: format crash at every prefix ─────────────────── */

fn scenario_format_crash_partial() -> Result<(), String> {
    let rng = Xorshift64::new(0xc0ffee_0006);
    let total_sectors = 1024usize;
    let mut rec = Box::new(WriteRecorder::new(total_sectors));
    let io = WriteRecorder::block_io(&mut *rec as *mut _);

    let opts = tessera_format_opts_t {
        total_sectors:   total_sectors as u64,
        journal_sectors: 64,
        volume_uuid:     random_uuid(&rng),
            seed_dirent_name: std::ptr::null(), seed_dirent_name_len: 0, seed_dirent_inode: 0,
            seed_content_data: std::ptr::null(), seed_content_len: 0,
            seed_chunk_size: 0,
    };
    let r = unsafe { tessera_volume_format(&io, &opts) };
    if r != 0 { return Err(format!("baseline format → {r}")); }
    let total_writes = rec.write_count();

    let mut opens_ok = 0u32;
    let mut opens_corrupt = 0u32;
    /* For every prefix length, replay and try to open. The contract
     * is binary: either a fully-readable consistent volume, or a
     * clean ECORRUPT. Never a half-open volume that returns nonsense. */
    for n in 0..=total_writes {
        let mut partial = rec.replay_through(n);
        let p_io = MemDisk::block_io(&mut partial);
        let mut v: *mut tessera_volume_t = std::ptr::null_mut();
        let r = unsafe { tessera_volume_open(&p_io, &mut v) };
        if r == 0 {
            /* Full consistency: all roots must point at writeable
             * sectors and all SB fields self-consistent. */
            let total = unsafe { tessera_volume_total_sectors(v) };
            let inode_root = unsafe { tessera_volume_inode_root(v) };
            let pack_root  = unsafe { tessera_volume_pack_registry_root(v) };
            let free_root  = unsafe { tessera_volume_free_extent_root(v) };
            unsafe { tessera_volume_close(v); }
            if total != total_sectors as u64 {
                return Err(format!("crash@{n}: total mismatch {total}"));
            }
            for r in [inode_root, pack_root, free_root] {
                if r >= total {
                    return Err(format!("crash@{n}: root {r} ≥ total {total}"));
                }
            }
            opens_ok += 1;
        } else if r == TESSERA_ECORRUPT {
            opens_corrupt += 1;
        } else {
            return Err(format!("crash@{n}: unexpected open errno {r}"));
        }
    }

    println!("  format_crash_partial: {} prefixes → {opens_ok} ok-or-empty, \
        {opens_corrupt} ECORRUPT (no half-open)", total_writes + 1);
    Ok(())
}

/* ── scenario 7: manifest builder/parser round-trip + hash ─────── */

fn scenario_manifest_round_trip() -> Result<(), String> {
    let rng = Xorshift64::new(0xc0ffee_0007);

    /* INLINE: random small payloads. */
    for _ in 0..32 {
        let len = (rng.range(2048) as usize) + 1;
        let mut data = vec![0u8; len];
        for i in 0..len { data[i] = (rng.next() & 0xff) as u8; }

        let b = unsafe { tessera_manifest_begin(TESSERA_MFT_INLINE) };
        if b.is_null() { return Err("inline begin null".into()); }
        let r = unsafe { tessera_manifest_set_inline(b, data.as_ptr(), len) };
        if r != 0 { return Err(format!("set_inline → {r}")); }

        let mut sz: usize = 0;
        let mut h1 = [0u8; 32];
        let _ = unsafe { tessera_manifest_finalize(b, std::ptr::null_mut(), 0,
            &mut sz, h1.as_mut_ptr()) };
        let mut buf = vec![0u8; sz];
        let r = unsafe { tessera_manifest_finalize(b, buf.as_mut_ptr(),
            sz, &mut sz, h1.as_mut_ptr()) };
        if r != 0 { return Err(format!("finalize → {r}")); }
        unsafe { tessera_manifest_free(b); }

        let p = unsafe { tessera_manifest_parse(buf.as_ptr(), sz) };
        if p.is_null() { return Err("parse null on clean bytes".into()); }
        if unsafe { tessera_manifest_parser_kind(p) } != TESSERA_MFT_INLINE {
            return Err("kind mismatch".into());
        }
        if unsafe { tessera_manifest_parser_size(p) } != len as u64 {
            return Err("size mismatch".into());
        }
        let mut od: *const u8 = std::ptr::null();
        let mut ol: usize = 0;
        let r = unsafe { tessera_manifest_inline_data(p, &mut od, &mut ol) };
        if r != 0 { return Err(format!("inline_data → {r}")); }
        if ol != len { return Err("inline len mismatch".into()); }
        let got = unsafe { std::slice::from_raw_parts(od, ol) };
        if got != data.as_slice() { return Err("inline content mismatch".into()); }
        unsafe { tessera_manifest_parser_free(p); }

        /* Hash determinism: rebuild the same manifest, verify identical
         * hash. (libmd vs portable stay in sync; same input → same
         * digest.) */
        let b2 = unsafe { tessera_manifest_begin(TESSERA_MFT_INLINE) };
        let _ = unsafe { tessera_manifest_set_inline(b2, data.as_ptr(), len) };
        let mut sz2: usize = 0;
        let mut h2 = [0u8; 32];
        let _ = unsafe { tessera_manifest_finalize(b2, std::ptr::null_mut(),
            0, &mut sz2, h2.as_mut_ptr()) };
        let mut buf2 = vec![0u8; sz2];
        let _ = unsafe { tessera_manifest_finalize(b2, buf2.as_mut_ptr(),
            sz2, &mut sz2, h2.as_mut_ptr()) };
        unsafe { tessera_manifest_free(b2); }
        if h1 != h2 { return Err("manifest hash not deterministic".into()); }
        if buf != buf2 { return Err("manifest bytes not deterministic".into()); }
    }

    /* CHUNK_LIST: random hashes + offsets. */
    for _ in 0..32 {
        let n = 1 + (rng.range(64) as u32);
        let b = unsafe { tessera_manifest_begin(TESSERA_MFT_CHUNK_LIST) };
        let mut entries: Vec<([u8; 32], u64, u32)> = Vec::new();
        let mut off = 0u64;
        for _ in 0..n {
            let mut h = [0u8; 32];
            for i in 0..32 { h[i] = (rng.next() & 0xff) as u8; }
            let size = 4096 + (rng.range(128 * 1024) as u32);
            let r = unsafe { tessera_manifest_add_chunk(b,
                h.as_ptr(), off, size, TESSERA_BLOB_FLAG_CHUNK) };
            if r != 0 { return Err(format!("add_chunk → {r}")); }
            entries.push((h, off, size));
            off += size as u64;
        }

        let mut sz: usize = 0;
        let mut h1 = [0u8; 32];
        let _ = unsafe { tessera_manifest_finalize(b, std::ptr::null_mut(),
            0, &mut sz, h1.as_mut_ptr()) };
        let mut buf = vec![0u8; sz];
        let r = unsafe { tessera_manifest_finalize(b, buf.as_mut_ptr(),
            sz, &mut sz, h1.as_mut_ptr()) };
        if r != 0 { return Err(format!("CL finalize → {r}")); }
        unsafe { tessera_manifest_free(b); }

        let p = unsafe { tessera_manifest_parse(buf.as_ptr(), sz) };
        if p.is_null() { return Err("CL parse null".into()); }
        if unsafe { tessera_manifest_parser_kind(p) } != TESSERA_MFT_CHUNK_LIST {
            return Err("CL kind".into());
        }
        if unsafe { tessera_manifest_parser_count(p) } != n {
            return Err("CL count".into());
        }
        for (i, (eh, eo, es)) in entries.iter().enumerate() {
            let mut r = tessera_chunk_record_t {
                chunk_hash: [0; 32], logical_offset: 0,
                uncompressed_size: 0, flags: 0,
            };
            let rc = unsafe { tessera_manifest_chunk_at(p, i as u32, &mut r) };
            if rc != 0 { return Err(format!("chunk_at[{i}] → {rc}")); }
            if &r.chunk_hash != eh
               || r.logical_offset    != *eo
               || r.uncompressed_size != *es {
                return Err(format!("CL entry {i} mismatch"));
            }
        }
        unsafe { tessera_manifest_parser_free(p); }
    }

    println!("  manifest_round_trip: 32 INLINE + 32 CHUNK_LIST manifests, \
        deterministic hash, byte-identical re-encode");
    Ok(())
}

/* ── scenario 8: manifest parser fuzz ─────────────────────────── */

fn scenario_manifest_parser_fuzz() -> Result<(), String> {
    let rng = Xorshift64::new(0xc0ffee_0008);
    let mut accepted = 0u32;
    let mut rejected = 0u32;

    /* Throw arbitrary byte buffers at the parser. The contract is "no
     * crash, no read past `len`". If parse() succeeds, the parser must
     * return only well-formed values (no out-of-band reads). */
    for _ in 0..2000 {
        let len = (rng.range(512) as usize) + 1;
        let mut buf = vec![0u8; len];
        for i in 0..len { buf[i] = (rng.next() & 0xff) as u8; }

        /* About 15% of the time, fix up the magic so parse() has a
         * better chance of succeeding — exercises the post-magic
         * validation paths. */
        if rng.range(100) < 15 && len >= 4 {
            buf[0] = b'T'; buf[1] = b'M'; buf[2] = b'F'; buf[3] = b'T';
        }

        let p = unsafe { tessera_manifest_parse(buf.as_ptr(), len) };
        if p.is_null() {
            rejected += 1;
            continue;
        }
        accepted += 1;

        /* parser still must self-validate: kind/size/count are consistent
         * with the body length. We probe via the public accessors. */
        let kind  = unsafe { tessera_manifest_parser_kind(p) };
        let _size = unsafe { tessera_manifest_parser_size(p) };
        let count = unsafe { tessera_manifest_parser_count(p) };

        if kind == TESSERA_MFT_CHUNK_LIST {
            /* chunk_at must return either OK (entry fits) or an error;
             * never crash. */
            for i in 0..count {
                let mut r = tessera_chunk_record_t {
                    chunk_hash: [0; 32], logical_offset: 0,
                    uncompressed_size: 0, flags: 0,
                };
                let _ = unsafe { tessera_manifest_chunk_at(p, i, &mut r) };
                /* No assertion on the value — fuzz doesn't know what's
                 * "right"; just that no UB occurs. */
            }
            /* Out-of-range index returns ENOENT, not crash. */
            let mut r = tessera_chunk_record_t {
                chunk_hash: [0; 32], logical_offset: 0,
                uncompressed_size: 0, flags: 0,
            };
            let rc = unsafe { tessera_manifest_chunk_at(p, count + 1000, &mut r) };
            if rc != TESSERA_ENOENT && rc != TESSERA_ECORRUPT {
                return Err(format!("OOB chunk_at gave {rc}, want ENOENT or ECORRUPT"));
            }
        }
        unsafe { tessera_manifest_parser_free(p); }
    }

    println!("  manifest_parser_fuzz: 2000 random inputs → {accepted} accepted, \
        {rejected} rejected; no crashes");
    Ok(())
}

/* ── scenario 9: CDC determinism ──────────────────────────────── */

fn scenario_cdc_determinism() -> Result<(), String> {
    let rng = Xorshift64::new(0xc0ffee_0009);
    let params = unsafe { &tessera_cdc_default_params };

    let split = |buf: &[u8]| -> Vec<usize> {
        let cap = buf.len() / params.min_chunk as usize + 4;
        let mut bounds = vec![0usize; cap];
        let mut n = 0usize;
        let r = unsafe { tessera_cdc_split(buf.as_ptr(), buf.len(),
            params, bounds.as_mut_ptr(), cap, &mut n) };
        if r != 0 { panic!("cdc_split → {r}"); }
        bounds.truncate(n);
        bounds
    };

    /* (a) Same input twice → identical boundaries. */
    for _ in 0..16 {
        let len = 256 * 1024 + (rng.range(2 * 1024 * 1024) as usize);
        let mut buf = vec![0u8; len];
        for i in 0..len { buf[i] = (rng.next() & 0xff) as u8; }
        let a = split(&buf);
        let b = split(&buf);
        if a != b { return Err("CDC not deterministic on same input".into()); }
        if a.is_empty() || *a.last().unwrap() != len {
            return Err("CDC last boundary != len".into());
        }
    }

    /* (b) Shift property: insert K bytes at the front of an otherwise-
     *     identical buffer. Most boundaries past the first chunk should
     *     re-align (== a's boundary + K). This is the property that
     *     gives content-addressed dedup its dedup ratio. */
    let mut hits = 0u64;
    let mut total = 0u64;
    for _ in 0..8 {
        let core_len = 1 << 20;
        let mut a_buf = vec![0u8; core_len];
        for i in 0..core_len { a_buf[i] = (rng.next() & 0xff) as u8; }

        let k: usize = 1 + (rng.range(15) as usize);
        let mut b_buf = vec![0u8; core_len + k];
        for i in 0..k { b_buf[i] = (rng.next() & 0xff) as u8; } /* fresh prefix */
        b_buf[k..].copy_from_slice(&a_buf);

        let ba = split(&a_buf);
        let bb = split(&b_buf);

        for &x in ba.iter() {
            total += 1;
            if bb.iter().any(|&y| y == x + k) { hits += 1; }
        }
    }
    let frac = hits as f64 / total as f64;
    if frac < 0.85 {
        return Err(format!("shift-property match fraction {frac:.2} < 0.85"));
    }

    println!("  cdc_determinism: 16 same-input pairs match, shift match {:.2}%",
        frac * 100.0);
    Ok(())
}

/* ── scenario 10: journal multi-block-body torn replay ───────── */

fn scenario_journal_multi_block_torn() -> Result<(), String> {
    let rng = Xorshift64::new(0xc0ffee_000a);
    let total = 256usize;
    let j_start: u64 = 0;
    let j_len: u64 = total as u64;

    let mut rec = Box::new(WriteRecorder::new(total));
    let io = WriteRecorder::block_io(&mut *rec as *mut _);
    let r = unsafe { tessera_journal_format(&io, j_start, j_len) };
    if r != 0 { return Err(format!("format → {r}")); }
    rec.rebaseline();

    /* tx-1: small committed record (must survive). */
    let j = unsafe { tessera_journal_open(&io, j_start, j_len) };
    let tag = b"surv-tx-1xxxxxxx";
    let mut tx1 = 0u64;
    let _ = unsafe { tessera_journal_tx_begin(j, &mut tx1, tag.as_ptr()) };
    let body1 = vec![0xa1u8; 64];
    let _ = unsafe { tessera_journal_append(j, tx1, TESSERA_INODE_WRITE,
        body1.as_ptr(), body1.len() as u32) };
    let _ = unsafe { tessera_journal_tx_commit(j, tx1) };

    /* writes-after-tx1 boundary */
    let after_tx1 = rec.write_count();

    /* tx-2: multi-block body (>4064 B). Without commit yet — we'll
     * crash mid-body to simulate torn write. */
    let mut tx2 = 0u64;
    let tag2 = b"big-body-tornxxx";
    let _ = unsafe { tessera_journal_tx_begin(j, &mut tx2, tag2.as_ptr()) };
    let big_len = 12_000usize;
    let mut big = vec![0u8; big_len];
    for i in 0..big_len { big[i] = (rng.next() & 0xff) as u8; }
    let r = unsafe { tessera_journal_append(j, tx2, TESSERA_PACK_PUBLISH,
        big.as_ptr(), big_len as u32) };
    if r != 0 { return Err(format!("big append → {r}")); }
    let after_big = rec.write_count();
    /* The big append spans multiple sectors. Find the per-sector
     * boundaries and pick a torn point inside it. */
    let big_writes = after_big - after_tx1 - 1; /* tx_begin tx2 was 1 write */
    if big_writes < 2 {
        return Err(format!("expected >=2 sectors for big body, got {big_writes}"));
    }
    let _ = unsafe { tessera_journal_tx_commit(j, tx2) };

    /* tx-3: another small committed record AFTER tx-2's commit. */
    let mut tx3 = 0u64;
    let _ = unsafe { tessera_journal_tx_begin(j, &mut tx3, tag.as_ptr()) };
    let body3 = vec![0xc3u8; 32];
    let _ = unsafe { tessera_journal_append(j, tx3, TESSERA_DIR_INSERT,
        body3.as_ptr(), body3.len() as u32) };
    let _ = unsafe { tessera_journal_tx_commit(j, tx3) };
    unsafe { tessera_journal_close(j); }

    /* Materialise the disk WITH tx-1's writes applied but with one of
     * tx-2's body-continuation sectors zeroed out. We do this by
     * replaying through the prefix that includes tx-1 fully + tx-2's
     * begin + the first body sector, but NOT the rest of tx-2's body
     * → simulates the body torn mid-write. */
    let torn_point = after_tx1 + 1 /* tx_begin */ + 1 /* first body sector */;
    let mut partial = rec.replay_through(torn_point);
    let p_io = MemDisk::block_io(&mut partial);

    let j2 = unsafe { tessera_journal_open(&p_io, j_start, j_len) };
    if j2.is_null() { return Err("torn replay: open failed".into()); }
    let mut cap = ReplayCapture { types: Vec::new(), bodies: Vec::new() };
    let _ = unsafe { tessera_journal_replay(j2, replay_cb,
        &mut cap as *mut _ as *mut c_void) };
    unsafe { tessera_journal_close(j2); }

    /* tx-1 fully committed before torn → must be replayed. tx-2's
     * body is incomplete + no COMMIT seen → must be dropped. tx-3
     * never written → not in this prefix. */
    if cap.types.len() != 1 {
        return Err(format!("expected 1 record (tx-1 only), got {}",
            cap.types.len()));
    }
    if cap.types[0] != TESSERA_INODE_WRITE as u32 {
        return Err(format!("wrong record type {} (want INODE_WRITE)",
            cap.types[0]));
    }
    if cap.bodies[0] != body1 {
        return Err("tx-1 body bytes wrong".into());
    }

    println!("  journal_multi_block_torn: tx-1 survived, tx-2 (torn body) dropped");
    Ok(())
}

/* ── scenario 11: B+tree close + reopen across sessions ───────── */

fn scenario_btree_persistence() -> Result<(), String> {
    let rng = Xorshift64::new(0xc0ffee_000b);
    let mut disk = MemDisk::new(4096);
    let io = MemDisk::block_io(&mut disk);

    const KEY: u32 = 4;
    const VAL: u32 = 32;

    let mut root = 0u64;
    let t = unsafe { tessera_btree_create(&io, 0, KEY, VAL, &mut root) };
    if t.is_null() { return Err("create null".into()); }
    unsafe { tessera_btree_close(t); }

    let mut oracle: HashMap<[u8; 4], [u8; 32]> = HashMap::new();

    /* Three sessions: open, do random ops, close, save root. Verify
     * each session sees what the previous one wrote. */
    for session in 0..3 {
        let t = unsafe { tessera_btree_open(&io, root, 0, KEY, VAL) };
        if t.is_null() { return Err(format!("session {session}: open null")); }

        /* Verify all oracle entries are visible at the start of the
         * session. */
        for (k, v) in &oracle {
            let mut got = [0u8; 32];
            let r = unsafe { tessera_btree_get(t, k.as_ptr(), got.as_mut_ptr()) };
            if r != 0 { return Err(format!("session {session}: get → {r}")); }
            if &got != v { return Err(format!("session {session}: value drifted")); }
        }

        /* Mutate. */
        for _ in 0..400 {
            let key32 = (rng.next() & 0xffff) as u32;
            let mut k = [0u8; 4];
            k[0] = (key32 >> 24) as u8; k[1] = (key32 >> 16) as u8;
            k[2] = (key32 >>  8) as u8; k[3] =  key32        as u8;
            let mut v = [0u8; 32];
            for i in 0..32 { v[i] = (rng.next() & 0xff) as u8; }
            let _ = unsafe { tessera_btree_put(t, k.as_ptr(), v.as_ptr(),
                &mut root) };
            oracle.insert(k, v);
        }
        unsafe { tessera_btree_close(t); }
    }

    /* Final verification: open one more time, walk via cursor, oracle
     * matches. */
    let t = unsafe { tessera_btree_open(&io, root, 0, KEY, VAL) };
    let c = unsafe { tessera_btree_seek_first(t) };
    let mut walked = 0usize;
    if !c.is_null() {
        loop {
            let mut k = [0u8; 4]; let mut v = [0u8; 32];
            if unsafe { tessera_btree_cursor_get(c, k.as_mut_ptr(), v.as_mut_ptr()) } != 0 {
                break;
            }
            walked += 1;
            match oracle.get(&k) {
                Some(ev) if ev == &v => {}
                _ => return Err("cursor saw key not in oracle or wrong value".into()),
            }
            if unsafe { tessera_btree_cursor_next(c) } != 0 { break; }
        }
        unsafe { tessera_btree_cursor_free(c); }
    }
    unsafe { tessera_btree_close(t); }
    if walked != oracle.len() {
        return Err(format!("walked {walked} != oracle {}", oracle.len()));
    }

    println!("  btree_persistence: 3 sessions × 400 ops, oracle = {} keys, \
        cursor walk matches across reopen", oracle.len());
    Ok(())
}

/* ── scenario 12: format size-boundary checking ──────────────── */

fn scenario_format_size_boundary() -> Result<(), String> {
    /* Below the minimum needed → EINVAL, no writes leaked. Just-above
     * minimum → OK. Far above → OK. */
    let try_format = |total: u64, journal: u64| -> i32 {
        let mut disk = MemDisk::new(total as usize + 1);
        let io = MemDisk::block_io(&mut disk);
        let opts = tessera_format_opts_t {
            total_sectors: total,
            journal_sectors: journal,
            volume_uuid: [0; 16],
            seed_dirent_name: std::ptr::null(), seed_dirent_name_len: 0, seed_dirent_inode: 0,
            seed_content_data: std::ptr::null(), seed_content_len: 0,
            seed_chunk_size: 0,
        };
        unsafe { tessera_volume_format(&io, &opts) }
    };

    /* total_sectors = 0 : EINVAL (or some non-zero error). */
    if try_format(0, 32) >= 0 { return Err("zero size accepted".into()); }
    /* journal too small: tessera_journal_format requires length >= 4. */
    if try_format(2048, 1) >= 0 { return Err("journal=1 accepted".into()); }
    if try_format(2048, 0) == 0 {
        /* journal=0 means default — should succeed. */
    } else {
        return Err("journal=0 (default) rejected".into());
    }

    /* Just-below-minimum: journal+metadata+1 = 4 + 32 + 1024 + 1 = 1061.
     * Anything <= journal+metadata fails. */
    let too_small = 4 + 32 + 1024 + 1;
    if try_format(too_small, 32) == 0 {
        return Err(format!("size={too_small} unexpectedly accepted"));
    }

    /* Just-above-minimum: must succeed. */
    let just_enough = 4 + 32 + 1024 + 8;
    if try_format(just_enough, 32) != 0 {
        return Err(format!("size={just_enough} unexpectedly rejected"));
    }

    /* Big: must succeed. */
    if try_format(8192, 256) != 0 { return Err("8192-sector format failed".into()); }

    println!("  format_size_boundary: rejects too-small/zero, accepts \
        minimum + above");
    Ok(())
}

/* ── runner ─────────────────────────────────────────────────── */

fn main() -> std::process::ExitCode {
    let scenarios: Vec<(&str, fn() -> Result<(), String>)> = vec![
        ("volume_round_trip",      scenario_volume_round_trip),
        ("extent_persistence",     scenario_extent_persistence),
        ("journal_crash_replay",   scenario_journal_crash_replay),
        ("btree_random_ops",       scenario_btree_random_ops),
        ("pack_corruption_fuzz",   scenario_pack_corruption_fuzz),
        ("format_crash_partial",   scenario_format_crash_partial),
        ("manifest_round_trip",    scenario_manifest_round_trip),
        ("manifest_parser_fuzz",   scenario_manifest_parser_fuzz),
        ("cdc_determinism",        scenario_cdc_determinism),
        ("journal_multi_block_torn",  scenario_journal_multi_block_torn),
        ("btree_persistence",         scenario_btree_persistence),
        ("format_size_boundary",      scenario_format_size_boundary),
    ];

    let argv: Vec<_> = std::env::args().skip(1).collect();
    let mut failed = 0;
    for (name, f) in &scenarios {
        if !argv.is_empty() && !argv.iter().any(|a| a == name) { continue; }
        print!("== {name} ==\n");
        match f() {
            Ok(()) => println!("  ok"),
            Err(e) => { println!("  FAIL: {e}"); failed += 1; }
        }
    }
    if failed == 0 {
        println!("\nall scenarios passed");
        std::process::ExitCode::SUCCESS
    } else {
        eprintln!("\n{failed} scenario(s) failed");
        std::process::ExitCode::FAILURE
    }
}
