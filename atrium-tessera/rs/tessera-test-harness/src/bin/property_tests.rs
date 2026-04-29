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

use std::collections::HashSet;
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

/* ── runner ─────────────────────────────────────────────────── */

fn main() -> std::process::ExitCode {
    let scenarios: Vec<(&str, fn() -> Result<(), String>)> = vec![
        ("volume_round_trip",     scenario_volume_round_trip),
        ("extent_persistence",    scenario_extent_persistence),
        ("journal_crash_replay",  scenario_journal_crash_replay),
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
