//! mkfs-tessera — format a file (or block device) as a Tessera volume.
//!
//! USAGE:
//!     mkfs-tessera [-j JOURNAL_SECTORS] PATH
//!     mkfs-tessera --create -s SIZE_MIB PATH
//!
//! `--create` allocates a new image of SIZE_MIB MiB at PATH (truncates
//! if it exists). Without `--create`, PATH must already exist; its
//! existing length determines the volume size (rounded down to a
//! 4 KiB sector boundary).

use std::process::ExitCode;

use tessera_sys::{tessera_format_opts_t, tessera_volume_format};
use tessera_tools::{
    fd_of, file_size, format_uuid, make_io, open_file_rw, random_uuid_v4,
    DiskCtx, SECTOR_SIZE,
};

fn usage() -> ! {
    eprintln!(
        "usage: mkfs-tessera [-j JOURNAL_SECTORS] [--create -s SIZE_MIB] PATH"
    );
    std::process::exit(2);
}

struct Args {
    path: String,
    journal_sectors: u64,
    create: Option<u64>, /* size in MiB if --create */
}

fn parse_args() -> Args {
    let mut a = Args { path: String::new(), journal_sectors: 256, create: None };
    let argv: Vec<_> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "-j" => {
                i += 1;
                if i >= argv.len() { usage(); }
                a.journal_sectors = argv[i].parse().unwrap_or_else(|_| usage());
            }
            "--create" => a.create = Some(0),
            "-s" => {
                i += 1;
                if i >= argv.len() { usage(); }
                let mib: u64 = argv[i].parse().unwrap_or_else(|_| usage());
                a.create = Some(mib);
            }
            "-h" | "--help" => usage(),
            arg if !arg.starts_with('-') => a.path = arg.to_string(),
            _ => usage(),
        }
        i += 1;
    }
    if a.path.is_empty() { usage(); }
    if a.create == Some(0) {
        eprintln!("error: --create requires -s SIZE_MIB");
        std::process::exit(2);
    }
    a
}

fn run() -> Result<(), String> {
    let args = parse_args();

    if let Some(mib) = args.create {
        let f = std::fs::OpenOptions::new()
            .read(true).write(true).create(true).truncate(true)
            .open(&args.path)
            .map_err(|e| format!("create {}: {e}", args.path))?;
        f.set_len(mib * 1024 * 1024)
            .map_err(|e| format!("set_len: {e}"))?;
    }

    let f = open_file_rw(&args.path)
        .map_err(|e| format!("open {}: {e}", args.path))?;
    let bytes = file_size(&f).map_err(|e| format!("stat: {e}"))?;
    let total_sectors = bytes / SECTOR_SIZE;
    if total_sectors < 4 + args.journal_sectors + 32 {
        return Err(format!(
            "image too small: {total_sectors} sectors; need at least \
             {} for the chosen journal", 4 + args.journal_sectors + 32));
    }

    let uuid = random_uuid_v4().map_err(|e| format!("uuid: {e}"))?;
    let mut ctx = DiskCtx { fd: fd_of(&f) };
    let io = make_io(&mut ctx);
    let opts = tessera_format_opts_t {
        total_sectors,
        journal_sectors: args.journal_sectors,
        volume_uuid: uuid,
    };
    let r = unsafe { tessera_volume_format(&io, &opts) };
    if r != 0 {
        return Err(format!("tessera_volume_format failed: errno={r}"));
    }
    /* drop f here flushes via O_SYNC */

    println!("formatted {}", args.path);
    println!("  total_sectors:   {total_sectors}");
    println!("  journal_sectors: {}", args.journal_sectors);
    println!("  uuid:            {}", format_uuid(&uuid));
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => { eprintln!("mkfs-tessera: {e}"); ExitCode::FAILURE }
    }
}
