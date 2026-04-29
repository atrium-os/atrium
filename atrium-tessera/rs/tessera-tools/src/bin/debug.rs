//! tessera-debug — open a Tessera volume and dump its high-level
//! structures. Read-only; intended as the operator's first stop when
//! something looks wrong with a volume.
//!
//! USAGE:
//!     tessera-debug PATH

use std::process::ExitCode;

use tessera_sys::{
    tessera_volume_close, tessera_volume_free_extent_root,
    tessera_volume_generation, tessera_volume_inode_root,
    tessera_volume_journal_length, tessera_volume_journal_start,
    tessera_volume_open, tessera_volume_pack_registry_root,
    tessera_volume_total_sectors, tessera_volume_uuid,
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
