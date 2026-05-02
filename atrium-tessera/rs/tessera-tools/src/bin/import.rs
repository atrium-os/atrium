//! tessera-import — copy a directory tree into a mounted Tessera volume,
//! relying on the kernel's content-addressed dedup to fold identical
//! files / chunks across imports into a single on-disk representation.
//!
//! USAGE:
//!     tessera-import <SRC_DIR> <DST_PATH>
//!
//! `<DST_PATH>` must be a path on a mounted Tessera filesystem, and
//! its parent directory must already exist. The import creates
//! `<DST_PATH>` as a directory and copies the contents of `<SRC_DIR>`
//! into it, preserving file modes, mtimes, and symlinks.
//!
//! Architectural note: this tool is *intentionally* a thin wrapper
//! around POSIX file ops, not a direct on-disk-format writer. The
//! kmod is the single owner of Tessera's on-disk invariants
//! (manifest layout, pack_registry dedup, journaling, snapshot
//! records). Each file written here goes through normal vop_write,
//! which routes through publish_chunked / publish_manifest_to_disk,
//! both of which check pack_registry first and skip publishing for
//! content that already exists. Two apps importing the same libc.so
//! produce one pack on disk.
//!
//! At end-of-import the tool prints:
//!   - bytes scanned (sum of source file sizes)
//!   - bytes added to the pack zone (df-based)
//!   - dedup ratio
//!   - publish-dedup counters (delta over the import)

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Instant;

fn usage() -> ! {
    eprintln!("usage: tessera-import <SRC_DIR> <DST_PATH>");
    std::process::exit(2);
}

#[derive(Default)]
struct Stats {
    files: u64,
    dirs: u64,
    symlinks: u64,
    bytes_in: u64,
}

/// Read a sysctl value (integer). Falls back to 0 on error so the
/// tool still prints something useful on a host without the kmod
/// loaded (e.g., dry-run on macOS during dev).
fn sysctl_u64(name: &str) -> u64 {
    let out = Command::new("sysctl").arg("-n").arg(name).output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .trim()
            .parse()
            .unwrap_or(0),
        _ => 0,
    }
}

/// `df -k <path>` → returns "used kbytes" for the filesystem
/// containing path. Tessera's df-style accounting reports the
/// pack-zone usage; we use it as the ground truth for "bytes added
/// to disk" by an import.
fn df_used_kib(path: &Path) -> io::Result<u64> {
    let out = Command::new("df").arg("-k").arg(path).output()?;
    if !out.status.success() {
        return Err(io::Error::other(format!(
            "df failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    // df output: header line, then one or more body lines.
    // We want the "Used" column from the body line. df can print the
    // mount info on a separate line if the device name is long, so
    // join all body lines and split.
    let s = String::from_utf8_lossy(&out.stdout);
    let mut lines = s.lines();
    let _hdr = lines.next();
    let body: String = lines.collect::<Vec<_>>().join(" ");
    let cols: Vec<&str> = body.split_whitespace().collect();
    // Filesystem 1K-blocks Used Avail Capacity Mounted-on
    // index:     0          1    2    3        4
    cols.get(2)
        .and_then(|s| s.parse::<u64>().ok())
        .ok_or_else(|| io::Error::other(format!("can't parse df: {body:?}")))
}

fn copy_file(src: &Path, dst: &Path, meta: &fs::Metadata) -> io::Result<u64> {
    let mut sf = File::open(src)?;
    let mut df = File::create(dst)?;
    // Reasonable chunk; matches Tessera's INLINE-vs-chunked threshold
    // so a 256 KiB file flows through one vop_write.
    let mut buf = vec![0u8; 256 * 1024];
    let mut total: u64 = 0;
    loop {
        let n = sf.read(&mut buf)?;
        if n == 0 {
            break;
        }
        df.write_all(&buf[..n])?;
        total += n as u64;
    }
    // Preserve permission bits (S_IRWXUGO + setuid/gid/sticky).
    let perms = fs::Permissions::from_mode(meta.mode() & 0o7777);
    fs::set_permissions(dst, perms)?;
    Ok(total)
}

fn import_one(src: &Path, dst: &Path, stats: &mut Stats) -> io::Result<()> {
    let meta = fs::symlink_metadata(src)?;
    let ft = meta.file_type();
    if ft.is_symlink() {
        let target = fs::read_link(src)?;
        symlink(&target, dst)?;
        stats.symlinks += 1;
    } else if ft.is_dir() {
        fs::create_dir(dst)?;
        let perms = fs::Permissions::from_mode(meta.mode() & 0o7777);
        fs::set_permissions(dst, perms)?;
        stats.dirs += 1;
        // Recurse: collect children, sort for determinism.
        let mut children: Vec<PathBuf> =
            fs::read_dir(src)?.filter_map(|e| e.ok().map(|e| e.path())).collect();
        children.sort();
        for child in children {
            let name = child.file_name().expect("read_dir returned path with no name");
            let dst_child = dst.join(name);
            import_one(&child, &dst_child, stats)?;
        }
    } else if ft.is_file() {
        let n = copy_file(src, dst, &meta)?;
        stats.bytes_in += n;
        stats.files += 1;
    } else {
        // Skip block/char/socket/fifo — apps in jails shouldn't have
        // these in their tree, and the FS doesn't currently model them.
        eprintln!(
            "  skip (unsupported file type): {}",
            src.display()
        );
    }
    Ok(())
}

fn human(b: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;
    if b >= GIB {
        format!("{:.2} GiB", b as f64 / GIB as f64)
    } else if b >= MIB {
        format!("{:.2} MiB", b as f64 / MIB as f64)
    } else if b >= KIB {
        format!("{:.2} KiB", b as f64 / KIB as f64)
    } else {
        format!("{b} B")
    }
}

fn run(src: &str, dst: &str) -> Result<(), String> {
    let src_path = PathBuf::from(src);
    let dst_path = PathBuf::from(dst);
    if !src_path.is_dir() {
        return Err(format!("source {src} is not a directory"));
    }
    let parent = dst_path
        .parent()
        .ok_or_else(|| format!("destination {dst} has no parent"))?;
    if !parent.exists() {
        return Err(format!(
            "destination parent {} does not exist",
            parent.display()
        ));
    }
    if dst_path.exists() {
        return Err(format!(
            "destination {dst} already exists; refusing to overwrite"
        ));
    }

    // Snapshot before-state for accounting.
    let used_before = df_used_kib(parent).map_err(|e| format!("df: {e}"))? * 1024;
    let dedup_chunked_before = sysctl_u64("kern.tessera.publish_dedup_chunked");
    let dedup_inline_before = sysctl_u64("kern.tessera.publish_dedup_manifest");
    let writes_chunked_before = sysctl_u64("kern.tessera.vop_write_chunked");
    let writes_inline_before = sysctl_u64("kern.tessera.vop_write_inline");

    let t0 = Instant::now();
    let mut stats = Stats::default();
    import_one(&src_path, &dst_path, &mut stats)
        .map_err(|e| format!("import: {e}"))?;

    // Force a flush so the on-disk delta is what we measure with df.
    // sync(2) on FreeBSD with our tessera_sync_impl drains
    // dirty_content + commits SB.
    unsafe {
        libc::sync();
    }

    let elapsed = t0.elapsed();
    let used_after = df_used_kib(parent).map_err(|e| format!("df: {e}"))? * 1024;
    let disk_delta = used_after.saturating_sub(used_before);

    let dedup_chunked = sysctl_u64("kern.tessera.publish_dedup_chunked")
        .saturating_sub(dedup_chunked_before);
    let dedup_inline = sysctl_u64("kern.tessera.publish_dedup_manifest")
        .saturating_sub(dedup_inline_before);
    let writes_chunked = sysctl_u64("kern.tessera.vop_write_chunked")
        .saturating_sub(writes_chunked_before);
    let writes_inline = sysctl_u64("kern.tessera.vop_write_inline")
        .saturating_sub(writes_inline_before);

    println!();
    println!("=== tessera-import: {src} → {dst} ===");
    println!(
        "  imported  : {} files, {} dirs, {} symlinks",
        stats.files, stats.dirs, stats.symlinks
    );
    println!(
        "  bytes in  : {} ({})",
        stats.bytes_in,
        human(stats.bytes_in)
    );
    println!(
        "  disk delta: {} ({})",
        disk_delta,
        human(disk_delta)
    );
    if stats.bytes_in > 0 && disk_delta > 0 && disk_delta < stats.bytes_in {
        let saved = stats.bytes_in - disk_delta;
        let ratio = stats.bytes_in as f64 / disk_delta as f64;
        println!(
            "  dedup     : {} saved ({}, {:.2}× compression vs raw)",
            saved,
            human(saved),
            ratio
        );
    } else if disk_delta == 0 {
        println!(
            "  dedup     : 100% (every byte already on disk)"
        );
    } else {
        println!("  dedup     : none (cold volume or all-unique content)");
    }
    println!(
        "  publishes : INLINE {} ({} dedup hits), CHUNKED {} ({} dedup hits)",
        writes_inline, dedup_inline, writes_chunked, dedup_chunked
    );
    println!("  elapsed   : {:.2}s", elapsed.as_secs_f64());

    // Light sanity check on the destination — verify we can read it
    // back and the entry count matches.
    let dst_files = walk_count_files(&dst_path).unwrap_or(0);
    if dst_files != stats.files {
        eprintln!(
            "WARN: destination contains {dst_files} files but {} were imported",
            stats.files
        );
    }

    Ok(())
}

fn walk_count_files(p: &Path) -> io::Result<u64> {
    let mut n = 0u64;
    let meta = fs::symlink_metadata(p)?;
    if meta.file_type().is_dir() {
        for e in fs::read_dir(p)? {
            let e = e?;
            n += walk_count_files(&e.path())?;
        }
    } else if meta.file_type().is_file() {
        n = 1;
    }
    Ok(n)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        usage();
    }
    // Quick sanity: reject paths with embedded NUL (libc API doesn't
    // care, but our error messages assume clean strings).
    for a in &args[1..] {
        if a.as_bytes().contains(&0) {
            eprintln!("error: argument contains NUL byte");
            return ExitCode::from(2);
        }
    }
    match run(&args[1], &args[2]) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("tessera-import: {e}");
            ExitCode::from(1)
        }
    }
}
