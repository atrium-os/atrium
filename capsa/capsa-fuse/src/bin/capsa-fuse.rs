//! atrium-capsa-fuse: serve one removable FAT/exFAT partition over FUSE.
//!
//! ```text
//! atrium-capsa-fuse --dev-fd N --fuse-fd M [--uid U --gid G] [--ro]
//! atrium-capsa-fuse --dev PATH --fuse-fd M ...
//! ```
//!
//! In Atrium jaild opens the partition and `/dev/fuse`, mounts fusefs on
//! the session's media directory from outside the jail, and starts this in
//! a jail with only those two descriptors (docs/spec/device-assignment.md
//! §4.6). FAT12/16/32 is served read-write unless `--ro`; exFAT read-only
//! for now.

use std::fs::File;
use std::os::fd::FromRawFd;
use std::process::ExitCode;

use capsa_fat::exfat::ExVolume;
use capsa_fat::{Aligned, BlockDev, Error, Volume};
use capsa_fuse::{serve, Options};

fn usage() -> ExitCode {
    eprintln!("usage: atrium-capsa-fuse (--dev PATH | --dev-fd N) --fuse-fd N [--uid U] [--gid G] [--ro]");
    ExitCode::from(2)
}

/// Sector size and length of a disk device, or of a plain file.
fn geometry(f: &File) -> (u64, u64) {
    #[cfg(target_os = "freebsd")]
    {
        use std::os::fd::AsRawFd;
        const DIOCGSECTORSIZE: libc::c_ulong = 0x4004_6480;
        const DIOCGMEDIASIZE: libc::c_ulong = 0x4008_6481;
        let (mut ss, mut ms): (libc::c_uint, libc::off_t) = (0, 0);
        // SAFETY: both ioctls fill the integer we pass; on a non-disk they fail.
        let ok = unsafe { libc::ioctl(f.as_raw_fd(), DIOCGSECTORSIZE, &mut ss) == 0 && libc::ioctl(f.as_raw_fd(), DIOCGMEDIASIZE, &mut ms) == 0 };
        if ok && ss > 0 && ms > 0 {
            return (u64::from(ss), ms as u64);
        }
    }
    (512, f.metadata().map(|m| m.len()).unwrap_or(0))
}

fn main() -> ExitCode {
    let mut dev: Option<File> = None;
    let mut fuse: Option<File> = None;
    let mut opts = Options::default();
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        let mut val = || it.next();
        let fd = |v: Option<String>| v.and_then(|s| s.parse::<i32>().ok()).filter(|&n| n > 2);
        match a.as_str() {
            // SAFETY: the caller handed us this descriptor and nothing else owns it.
            "--dev-fd" => match fd(val()) {
                Some(n) => dev = Some(unsafe { File::from_raw_fd(n) }),
                None => return usage(),
            },
            "--dev" => match val().map(|p| std::fs::OpenOptions::new().read(true).write(true).open(&p).or_else(|_| File::open(&p))) {
                Some(Ok(f)) => dev = Some(f),
                _ => return usage(),
            },
            // SAFETY: as above.
            "--fuse-fd" => match fd(val()) {
                Some(n) => fuse = Some(unsafe { File::from_raw_fd(n) }),
                None => return usage(),
            },
            "--uid" => match val().and_then(|s| s.parse().ok()) {
                Some(u) => opts.uid = u,
                None => return usage(),
            },
            "--gid" => match val().and_then(|s| s.parse().ok()) {
                Some(g) => opts.gid = g,
                None => return usage(),
            },
            "--ro" => opts.read_only = true,
            _ => return usage(),
        }
    }
    let (Some(dev), Some(fuse)) = (dev, fuse) else { return usage() };
    let (sector, len) = geometry(&dev);
    let Ok(mut dev) = Aligned::new(dev, sector, len) else { return usage() };
    let mut head = [0u8; 11];
    if let Err(e) = dev.read_at(0, &mut head) {
        eprintln!("atrium-capsa-fuse: {e}");
        return ExitCode::FAILURE;
    }
    let r = if &head[3..11] == b"EXFAT   " {
        match ExVolume::open(dev) {
            Ok(v) => {
                eprintln!("atrium-capsa-fuse: exFAT, served read-only");
                serve(v, fuse, opts)
            }
            Err(e) => return fail(e),
        }
    } else {
        match Volume::open(dev) {
            Ok(v) => serve(v, fuse, opts),
            Err(e) => return fail(e),
        }
    };
    match r {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("atrium-capsa-fuse: /dev/fuse: {e}");
            ExitCode::FAILURE
        }
    }
}

fn fail(e: Error) -> ExitCode {
    eprintln!("atrium-capsa-fuse: {e}");
    ExitCode::FAILURE
}
