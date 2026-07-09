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
        "usage: mkfs-tessera [-j JOURNAL_SECTORS] [--create -s SIZE_MIB] \\\n\
         \x20             [--seed-file NAME [--seed-inode N]] \\\n\
         \x20             [--hash-alg sha256|blake3] PATH"
    );
    std::process::exit(2);
}

struct Args {
    path: String,
    journal_sectors: u64,
    create: Option<u64>, /* size in MiB if --create */
    seed_name: Option<String>,
    seed_inode: u64,
    seed_content: Option<String>,
    seed_chunk_size: u32,
    hash_alg: u32,
}

fn parse_args() -> Args {
    let mut a = Args {
        path: String::new(), journal_sectors: 256, create: None,
        seed_name: None, seed_inode: 1000, seed_content: None,
        seed_chunk_size: 0, hash_alg: 1, /* default blake3 (2026-07-09) */
    };
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
            "--seed-file" => {
                i += 1;
                if i >= argv.len() { usage(); }
                a.seed_name = Some(argv[i].clone());
            }
            "--seed-inode" => {
                i += 1;
                if i >= argv.len() { usage(); }
                a.seed_inode = argv[i].parse().unwrap_or_else(|_| usage());
            }
            "--seed-content" => {
                i += 1;
                if i >= argv.len() { usage(); }
                a.seed_content = Some(argv[i].clone());
            }
            "--seed-chunk-size" => {
                i += 1;
                if i >= argv.len() { usage(); }
                a.seed_chunk_size = argv[i].parse().unwrap_or_else(|_| usage());
            }
            "--hash-alg" => {
                i += 1;
                if i >= argv.len() { usage(); }
                a.hash_alg = match argv[i].as_str() {
                    "sha256" => 0,
                    "blake3" => 1,
                    _ => usage(),
                };
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
        // Block / character devices: skip truncate + set_len; the
        // kernel rejects both. Detect by trying to stat the path —
        // if it exists and its st_mode is BLK/CHR, skip allocation
        // and let the existing device geometry drive total_sectors
        // below (file_size uses DIOCGMEDIASIZE on FreeBSD).
        let is_dev = std::fs::metadata(&args.path)
            .map(|m| {
                use std::os::unix::fs::FileTypeExt;
                m.file_type().is_block_device() ||
                m.file_type().is_char_device()
            })
            .unwrap_or(false);
        if !is_dev {
            let f = std::fs::OpenOptions::new()
                .read(true).write(true).create(true).truncate(true)
                .open(&args.path)
                .map_err(|e| format!("create {}: {e}", args.path))?;
            f.set_len(mib * 1024 * 1024)
                .map_err(|e| format!("set_len: {e}"))?;
        }
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

    /* Seed-name and seed-content bytes must outlive the format call. */
    let seed_bytes: Option<Vec<u8>> = args.seed_name.as_ref()
        .map(|s| s.as_bytes().to_vec());
    let (seed_ptr, seed_len) = match &seed_bytes {
        Some(b) => (b.as_ptr(), b.len() as u16),
        None    => (std::ptr::null(), 0u16),
    };
    let content_bytes: Option<Vec<u8>> = args.seed_content.as_ref()
        .map(|s| s.as_bytes().to_vec());
    let (content_ptr, content_len) = match &content_bytes {
        Some(b) => (b.as_ptr(), b.len()),
        None    => (std::ptr::null(), 0usize),
    };

    let opts = tessera_format_opts_t {
        total_sectors,
        journal_sectors: args.journal_sectors,
        volume_uuid: uuid,
        seed_dirent_name:     seed_ptr,
        seed_dirent_name_len: seed_len,
        seed_dirent_inode:    args.seed_inode,
        seed_content_data:    content_ptr,
        seed_content_len:     content_len,
        seed_chunk_size:      args.seed_chunk_size,
        hash_alg:             args.hash_alg,
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
    println!("  hash_alg:        {}", if args.hash_alg == 1 { "blake3" } else { "sha256" });
    if let Some(ref name) = args.seed_name {
        println!("  seeded:          /{} -> inode {} ({} bytes content)",
            name, args.seed_inode, content_len);
    }
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => { eprintln!("mkfs-tessera: {e}"); ExitCode::FAILURE }
    }
}
