//! tessera-defrag — offline data-zone fragmentation report.
//!
//! Reports data-zone health for an UNMOUNTED volume: free-space fragmentation
//! (how carved-up the free space is) and genuinely-ganged packs (bodies split
//! across >1 data extent, read via a PEL chain).
//!
//! This is a REPORT, not a mover. Recovery is already handled ONLINE by the
//! kmod's v2 repack engine, which de-gangs multi-extent packs on a background
//! trigger (past `kern.tessera.repack_threshold`) and in a bounded mount-time
//! pass — so remounting a fragmented volume reconsolidates it. And in practice
//! the data zone resists fragmentation to begin with: the extent allocator
//! coalesces on free(), best-fits allocations, and batches blobs into shared
//! packs, so free space stays near-contiguous under normal churn. This tool
//! exists to CONFIRM that (or flag the rare pathological case) without mounting.
//! An offline mover would only duplicate the online engine, so there is none —
//! to recover a genuinely-fragmented volume, mount it and let the engine run.

use std::process::ExitCode;
use tessera_sys::*;
use tessera_tools::{fd_of, make_io, open_file_ro, DiskCtx};

const FLAG_MULTI_EXTENT: u32 = 1 << 2;
const SECTOR: u64 = 4096;
const PEL_MAGIC: u64 = 0x315056454C455054; // "TPELEV01"

/// Read one 4096-byte sector via pread. Returns zeroed buf on short read.
fn pread_sector(fd: i32, sector: u64) -> [u8; 4096] {
    let mut buf = [0u8; 4096];
    unsafe {
        libc::pread(fd, buf.as_mut_ptr() as *mut libc::c_void, 4096,
            (sector * SECTOR) as i64);
    }
    buf
}

/// Follow a pack's PEL chain and return the true number of data extents.
/// A MULTI_EXTENT-flagged pack whose chain sums to 1 is only *flagged* — it
/// occupies a single contiguous extent (the multi-extent allocator returned one
/// run) and pays a 1-sector PEL for nothing; it is NOT genuinely ganged.
fn pel_extent_count(fd: i32, head_pel: u64) -> u64 {
    let mut total = 0u64;
    let mut sec = head_pel;
    let mut guard = 0;
    while sec != 0 && guard < 512 {
        let b = pread_sector(fd, sec);
        if u64::from_le_bytes(b[0..8].try_into().unwrap()) != PEL_MAGIC { break; }
        total += u32::from_le_bytes(b[12..16].try_into().unwrap()) as u64;
        sec = u64::from_le_bytes(b[24..32].try_into().unwrap());
        guard += 1;
    }
    total
}

/// One free run (start_sector, length_sectors), read from the free-extent tree.
/// The tree stores raw native-endian u64 key/value (see core/src/extent.c), so
/// decode little-endian on the platforms we target.
fn read_free_extents(io: &tessera_block_io_t, root: u64) -> Result<Vec<(u64, u64)>, String> {
    let mut out = Vec::new();
    if root == 0 { return Ok(out); }
    unsafe {
        let t = tessera_btree_open(io, root, TESSERA_BTREE_KIND_FREE_EXT, 8, 8);
        if t.is_null() { return Err("open free-extent tree".into()); }
        let c = tessera_btree_seek_first(t);
        if !c.is_null() {
            loop {
                let mut k = [0u8; 8];
                let mut v = [0u8; 8];
                if tessera_btree_cursor_get(c, k.as_mut_ptr(), v.as_mut_ptr()) != 0 { break; }
                out.push((u64::from_le_bytes(k), u64::from_le_bytes(v)));
                if tessera_btree_cursor_next(c) != 0 { break; }
            }
            tessera_btree_cursor_free(c);
        }
        tessera_btree_close(t);
    }
    Ok(out)
}

/// Pack-registry summary counts.
#[derive(Default)]
struct RegStats {
    total: u64,        // all packs
    flagged: u64,      // MULTI_EXTENT flag set
    ganged: u64,       // genuinely >1 data extent (PEL chain sum > 1)
    flagged_single: u64, // flagged but 1 extent — pure PEL overhead
    pel_sectors: u64,  // total PEL sectors (chain length) across flagged packs
    max_extents: u64,  // worst genuinely-ganged pack
}

fn scan_registry(fd: i32, io: &tessera_block_io_t, root: u64) -> Result<RegStats, String> {
    let mut s = RegStats::default();
    if root == 0 { return Ok(s); }
    unsafe {
        let t = tessera_btree_open(io, root, TESSERA_BTREE_KIND_PACK_REG, 16,
            TESSERA_REGISTRY_ENTRY_SIZE);
        if t.is_null() { return Err("open pack-registry tree".into()); }
        let c = tessera_btree_seek_first(t);
        if !c.is_null() {
            let mut key = [0u8; 16];
            let mut val = vec![0u8; TESSERA_REGISTRY_ENTRY_SIZE as usize];
            loop {
                if tessera_btree_cursor_get(c, key.as_mut_ptr(), val.as_mut_ptr()) != 0 { break; }
                s.total += 1;
                let flags = u32::from_le_bytes(val[60..64].try_into().unwrap());
                if flags & FLAG_MULTI_EXTENT != 0 {
                    s.flagged += 1;
                    let head_pel = u64::from_le_bytes(val[16..24].try_into().unwrap());
                    let ext = pel_extent_count(fd, head_pel);
                    // count PEL sectors in the chain for the overhead figure
                    let mut sec = head_pel; let mut guard = 0;
                    while sec != 0 && guard < 512 {
                        let b = pread_sector(fd, sec);
                        if u64::from_le_bytes(b[0..8].try_into().unwrap()) != PEL_MAGIC { break; }
                        s.pel_sectors += 1;
                        sec = u64::from_le_bytes(b[24..32].try_into().unwrap());
                        guard += 1;
                    }
                    if ext > 1 { s.ganged += 1; s.max_extents = s.max_extents.max(ext); }
                    else { s.flagged_single += 1; }
                }
                if tessera_btree_cursor_next(c) != 0 { break; }
            }
            tessera_btree_cursor_free(c);
        }
        tessera_btree_close(t);
    }
    Ok(s)
}

fn run(path: &str) -> Result<i32, String> {
    let f = open_file_ro(path).map_err(|e| format!("open {path}: {e}"))?;
    let mut ctx = DiskCtx::ro(fd_of(&f));
    let io = make_io(&mut ctx);

    let mut v: *mut tessera_volume_t = std::ptr::null_mut();
    if unsafe { tessera_volume_open(&io, &mut v) } != 0 {
        return Err("SUPERBLOCK INVALID (tessera_volume_open failed)".into());
    }
    let pz_start = unsafe { tessera_volume_pack_zone_start(v) };
    let pz_len   = unsafe { tessera_volume_pack_zone_length(v) };
    let free_root = unsafe { tessera_volume_free_extent_root(v) };
    let pack_root = unsafe { tessera_volume_pack_registry_root(v) };
    let generation = unsafe { tessera_volume_generation(v) };

    let frees = read_free_extents(&io, free_root)?;
    let rs = scan_registry(fd_of(&f), &io, pack_root)?;
    unsafe { tessera_volume_close(v) };

    let free_total: u64 = frees.iter().map(|(_, l)| *l).sum();
    let largest: u64 = frees.iter().map(|(_, l)| *l).max().unwrap_or(0);
    let nruns = frees.len() as u64;
    let used = pz_len.saturating_sub(free_total);
    // Fragmentation index: 1 - largest_run/total_free. 0 = one contiguous run
    // (pristine); →1 = free space shattered into many small runs.
    let frag_pct = if free_total > 0 {
        100.0 * (1.0 - largest as f64 / free_total as f64)
    } else { 0.0 };

    println!("tessera-defrag: {path}");
    println!("  generation:      {generation}");
    println!("  pack zone:       {pz_len} sectors @ {pz_start}  ({} MiB)",
        pz_len * 4096 / (1024 * 1024));
    println!("  used / free:     {used} / {free_total} sectors  ({:.1}% full)",
        if pz_len > 0 { 100.0 * used as f64 / pz_len as f64 } else { 0.0 });
    println!("  free runs:       {nruns}  (largest {largest} sectors = {} MiB)",
        largest * 4096 / (1024 * 1024));
    println!("  packs:           {}  ({} genuinely ganged >1 extent{}, {} flagged-single = {} sectors PEL overhead)",
        rs.total, rs.ganged,
        if rs.ganged > 0 { format!(", worst {} extents", rs.max_extents) } else { String::new() },
        rs.flagged_single, rs.pel_sectors);
    println!("  free-space frag: {frag_pct:.1}%  (0% = free space is one run, 100% = shattered)");

    // Run-size histogram (log2 buckets) — shows whether free space is a few big
    // runs or a swarm of dust.
    if nruns > 0 {
        let mut buckets = [0u64; 12]; // <8, <16, ... , >=8192 sectors
        for (_, l) in &frees {
            let b = (63 - (*l).max(1).leading_zeros()).min(11) as usize;
            buckets[b] += 1;
        }
        print!("  run sizes:      ");
        for (i, cnt) in buckets.iter().enumerate() {
            if *cnt > 0 { print!(" 2^{i}:{cnt}"); }
        }
        println!();
    }

    if rs.ganged > 0 || (nruns > 4 && frag_pct > 40.0) {
        println!("  → fragmented: {} genuinely-ganged pack(s) / free space in {nruns} runs.", rs.ganged);
        println!("    NOTE: the kmod's v2 repack engine de-gangs multi-extent packs online");
        println!("    (background at >{} + a mount-time pass), so a remount usually clears this.", 50);
    } else if rs.flagged_single > 0 {
        println!("  → healthy: free space is contiguous, no genuinely-ganged packs. {} pack(s)",
            rs.flagged_single);
        println!("    are flagged MULTI_EXTENT but occupy one extent — {} sectors of spurious PEL",
            rs.pel_sectors);
        println!("    overhead (arises only from the force_multi_extent debug knob, not organically).");
    } else {
        println!("  → data zone is healthy — no fragmentation.");
    }
    Ok(0)
}

fn main() -> ExitCode {
    let args: Vec<_> = std::env::args().collect();
    let path = args.iter().skip(1).find(|a| !a.starts_with('-'));
    let path = match path {
        Some(p) => p.clone(),
        None => {
            eprintln!("usage: tessera-defrag PATH");
            eprintln!("  offline data-zone fragmentation report (volume must be UNMOUNTED)");
            eprintln!("  report-only: recovery (de-ganging) runs online in the kmod repack");
            eprintln!("  engine — mount a fragmented volume to reconsolidate it.");
            return ExitCode::from(2);
        }
    };
    match run(&path) {
        Ok(code) => ExitCode::from(code as u8),
        Err(e) => { eprintln!("tessera-defrag: {e}"); ExitCode::from(1) }
    }
}
