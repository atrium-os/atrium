//! tessera-debug — open a Tessera volume and dump its high-level
//! structures. Read-only; intended as the operator's first stop when
//! something looks wrong with a volume.
//!
//! USAGE:
//!     tessera-debug PATH

use std::process::ExitCode;

use tessera_sys::{
    tessera_volume_active_slot_count, tessera_volume_close,
    tessera_volume_encryption_flags, tessera_volume_free_extent_root,
    tessera_volume_generation, tessera_volume_inode_root,
    tessera_volume_journal_length, tessera_volume_journal_start,
    tessera_volume_meta_reserve_bump, tessera_volume_meta_reserve_length,
    tessera_volume_meta_reserve_start, tessera_volume_open,
    tessera_volume_pack_registry_root, tessera_volume_pack_zone_length,
    tessera_volume_pack_zone_start, tessera_volume_snapshots_gen,
    tessera_volume_snapshots_root, tessera_volume_total_sectors,
    tessera_volume_uuid,
};
use tessera_tools::{
    fd_of, file_size, format_uuid, make_io, open_file_ro, DiskCtx, SECTOR_SIZE,
};

fn run(path: &str) -> Result<(), String> {
    let f = open_file_ro(path).map_err(|e| format!("open {path}: {e}"))?;
    let total_bytes = file_size(&f).map_err(|e| format!("stat: {e}"))?;

    let mut ctx = DiskCtx { fd: fd_of(&f) };
    let io = make_io(&mut ctx);
    let mut v: *mut tessera_sys::tessera_volume_t = std::ptr::null_mut();
    let r = unsafe { tessera_volume_open(&io, &mut v) };
    if r != 0 {
        return Err(format!("tessera_volume_open failed: errno={r}"));
    }

    let total_sectors = unsafe { tessera_volume_total_sectors(v) };
    let generation    = unsafe { tessera_volume_generation(v) };
    let inode_root    = unsafe { tessera_volume_inode_root(v) };
    let pack_root     = unsafe { tessera_volume_pack_registry_root(v) };
    let free_root     = unsafe { tessera_volume_free_extent_root(v) };
    let j_start       = unsafe { tessera_volume_journal_start(v) };
    let j_length      = unsafe { tessera_volume_journal_length(v) };
    let uuid_ptr      = unsafe { tessera_volume_uuid(v) };
    let mut uuid = [0u8; 16];
    if !uuid_ptr.is_null() {
        unsafe {
            std::ptr::copy_nonoverlapping(uuid_ptr, uuid.as_mut_ptr(), 16);
        }
    }

    println!("Tessera volume: {path}");
    println!("  file size:        {total_bytes} bytes ({} sectors)",
        total_bytes / SECTOR_SIZE);
    println!("  ── superblock ──────────────────────────────────────");
    println!("  total_sectors:    {total_sectors}");
    println!("  generation:       {generation}");
    println!("  uuid:             {}", format_uuid(&uuid));
    println!("  ── on-disk roots ───────────────────────────────────");
    println!("  journal:          start={j_start} length={j_length}");
    println!("  inode_root:       sector {inode_root}");
    println!("  pack_registry:    sector {pack_root}");
    println!("  free_extent_root: sector {free_root}");

    let snap_root = unsafe { tessera_volume_snapshots_root(v) };
    let snap_gen  = unsafe { tessera_volume_snapshots_gen(v) };
    let mr_start  = unsafe { tessera_volume_meta_reserve_start(v) };
    let mr_len    = unsafe { tessera_volume_meta_reserve_length(v) };
    let mr_bump   = unsafe { tessera_volume_meta_reserve_bump(v) };
    let enc_flags = unsafe { tessera_volume_encryption_flags(v) };
    let act_slots = unsafe { tessera_volume_active_slot_count(v) };

    println!("  ── v2 snapshots ────────────────────────────────────");
    if snap_root == 0 {
        println!("  snapshots_tree:   (uninitialised — pre-v2 mount)");
    } else {
        println!("  snapshots_root:   sector {snap_root}");
        println!("  snapshots_gen:    {snap_gen}");
    }

    println!("  ── meta-reserve ────────────────────────────────────");
    let mr_used = mr_bump.saturating_sub(mr_start);
    let mr_pct  = if mr_len > 0 { (mr_used * 100) / mr_len } else { 0 };
    println!("  range:            sectors {mr_start}..{}",
        mr_start + mr_len);
    println!("  bump pointer:     sector {mr_bump} ({mr_used}/{mr_len} used, {mr_pct}%)");

    let pz_start = unsafe { tessera_volume_pack_zone_start(v) };
    let pz_len   = unsafe { tessera_volume_pack_zone_length(v) };
    println!("  ── pack zone ───────────────────────────────────────");
    println!("  range:            sectors {pz_start}..{} ({} sectors / {} MiB)",
        pz_start + pz_len, pz_len, pz_len / 256);

    println!("  ── v3 encryption ───────────────────────────────────");
    if enc_flags == 0 {
        println!("  encryption:       off (no slots)");
    } else {
        let aes  = if (enc_flags & 0x1) != 0 { "AES-XTS " } else { "" };
        let conv = if (enc_flags & 0x2) != 0 { "convergent " } else { "" };
        println!("  encryption:       {aes}{conv}(flags=0x{enc_flags:04x})");
        println!("  active slots:     {act_slots}/8");
    }

    unsafe { tessera_volume_close(v); }
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 2 {
        eprintln!("usage: tessera-debug PATH");
        return ExitCode::from(2);
    }
    match run(&args[1]) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => { eprintln!("tessera-debug: {e}"); ExitCode::FAILURE }
    }
}
