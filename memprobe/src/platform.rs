//! Finding and reading a process's resident anonymous pages.
//!
//! "Anonymous" = what the pageout daemon would send to swap, and so what a
//! compressed-RAM store would hold: heap, stacks, anonymous mmaps and the
//! copy-on-write pages of private file mappings. Clean file pages are dropped,
//! not swapped, so they are out of scope.
//!
//! ★ Reading must not change what it measures. On Linux `/proc/<pid>/pagemap`
//! says which pages are present and exclusively anonymous, and only those are
//! read. FreeBSD has no per-page residency query for another process, and
//! reading a never-touched anonymous page through ptrace(PT_IO) would fault a
//! fresh zero page into the target. So on FreeBSD only fully resident regions
//! are read by default, and the skipped bytes are reported as coverage, never
//! silently dropped. The kernel's own zram counters remain the ground truth
//! for what actually reaches the store.

use std::io;

pub const PAGE: u64 = 4096;

#[derive(Debug, Clone)]
pub struct Region {
    pub start: u64,
    pub end: u64,
}

#[derive(Debug, Default, Clone)]
pub struct Coverage {
    /// Resident anonymous bytes the probe read.
    pub read: u64,
    /// Resident anonymous bytes it knew about but did not read (FreeBSD:
    /// partially resident regions).
    pub skipped: u64,
    /// Bytes already in swap (Linux pagemap tells us; not read).
    pub swapped: u64,
    /// FreeBSD `--fault-partial`: non-resident bytes the read faulted in.
    /// They are never-touched anonymous pages, so they read as zero; the
    /// caller subtracts them from the zero-page count.
    pub faulted: u64,
}

pub fn list_procs() -> Vec<(i32, String)> {
    imp::list_procs()
}

/// Calls `f` with every resident anonymous page of `pid` that the platform
/// can read without perturbing it.
pub fn for_each_anon_page(
    pid: i32,
    opts: &ReadOpts,
    f: &mut dyn FnMut(&[u8]),
) -> io::Result<Coverage> {
    imp::for_each_anon_page(pid, opts, f)
}

#[derive(Debug, Default, Clone)]
pub struct ReadOpts {
    /// Analyse one page in N (1 = all).
    pub sample: u64,
    /// FreeBSD only: also read partially resident regions, faulting in their
    /// non-resident pages. Perturbs the target; off by default.
    #[cfg_attr(not(target_os = "freebsd"), allow(dead_code))]
    pub fault_partial: bool,
}

#[cfg(target_os = "linux")]
mod imp {
    use super::*;
    use std::fs::{self, File};
    use std::os::unix::fs::FileExt;

    pub fn list_procs() -> Vec<(i32, String)> {
        let mut v = Vec::new();
        if let Ok(rd) = fs::read_dir("/proc") {
            for e in rd.flatten() {
                if let Ok(pid) = e.file_name().to_string_lossy().parse::<i32>() {
                    let comm = fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
                    v.push((pid, comm.trim().to_string()));
                }
            }
        }
        v.sort();
        v
    }

    /// Private writable mappings: anonymous ones and the COW pages of private
    /// file mappings. pagemap then filters to exclusively-anonymous pages.
    fn regions(pid: i32) -> io::Result<Vec<Region>> {
        let maps = fs::read_to_string(format!("/proc/{pid}/maps"))?;
        let mut v = Vec::new();
        for line in maps.lines() {
            let mut it = line.split_whitespace();
            let (Some(range), Some(perms)) = (it.next(), it.next()) else { continue };
            let path = it.nth(3).unwrap_or("");
            let p = perms.as_bytes();
            if p.len() < 4 || p[0] != b'r' || p[1] != b'w' || p[3] != b'p' {
                continue;
            }
            if path.starts_with("[vvar") || path.starts_with("[vdso") || path.starts_with("[vsyscall") {
                continue;
            }
            let Some((a, b)) = range.split_once('-') else { continue };
            let (Ok(start), Ok(end)) = (u64::from_str_radix(a, 16), u64::from_str_radix(b, 16)) else { continue };
            v.push(Region { start, end });
        }
        Ok(v)
    }

    const PM_PRESENT: u64 = 1 << 63;
    const PM_SWAP: u64 = 1 << 62;
    const PM_FILE_OR_SHARED: u64 = 1 << 61;

    pub fn for_each_anon_page(pid: i32, opts: &ReadOpts, f: &mut dyn FnMut(&[u8])) -> io::Result<Coverage> {
        let regions = regions(pid)?;
        let pagemap = File::open(format!("/proc/{pid}/pagemap"))?;
        let mem = File::open(format!("/proc/{pid}/mem"))?;
        let sample = opts.sample.max(1);
        let mut cov = Coverage::default();
        let mut pm = Vec::<u8>::new();
        let mut buf = vec![0u8; PAGE as usize];
        let mut idx: u64 = 0;
        for r in regions {
            let n = ((r.end - r.start) / PAGE) as usize;
            pm.resize(n * 8, 0);
            if pagemap.read_exact_at(&mut pm, r.start / PAGE * 8).is_err() {
                continue;
            }
            for i in 0..n {
                let e = u64::from_ne_bytes(pm[i * 8..i * 8 + 8].try_into().unwrap());
                if e & PM_SWAP != 0 {
                    cov.swapped += PAGE;
                    continue;
                }
                if e & PM_PRESENT == 0 || e & PM_FILE_OR_SHARED != 0 {
                    continue;
                }
                idx += 1;
                if idx % sample != 0 {
                    continue;
                }
                // A page can vanish between pagemap and the read (the target
                // unmapped it); skip it rather than fail the process.
                if mem.read_exact_at(&mut buf, r.start + i as u64 * PAGE).is_ok() {
                    cov.read += PAGE;
                    f(&buf);
                }
            }
        }
        Ok(cov)
    }
}

#[cfg(target_os = "freebsd")]
mod imp {
    use super::*;
    use std::process::Command;

    pub fn list_procs() -> Vec<(i32, String)> {
        let out = // BSD ps takes everything after `=` as the header, so "pid=,comm=" would
        // print one column; each field needs its own -o.
        Command::new("ps").args(["-ax", "-o", "pid=", "-o", "comm="]).output();
        let mut v = Vec::new();
        if let Ok(o) = out {
            for line in String::from_utf8_lossy(&o.stdout).lines() {
                let line = line.trim();
                if let Some((p, c)) = line.split_once(char::is_whitespace) {
                    if let Ok(pid) = p.parse() {
                        v.push((pid, c.trim().to_string()));
                    }
                }
            }
        }
        v.sort();
        v
    }

    struct VmRegion {
        r: Region,
        resident_pages: u64,
    }

    /// `procstat -v`: PID START END PRT RES PRES REF SHD FLAG TP PATH.
    /// TP `sw` (swap-backed) and `df` (default, not yet swap-backed) are the
    /// anonymous objects.
    fn regions(pid: i32) -> io::Result<Vec<VmRegion>> {
        let o = Command::new("procstat").args(["-v", &pid.to_string()]).output()?;
        if !o.status.success() {
            return Err(io::Error::other(String::from_utf8_lossy(&o.stderr).trim().to_string()));
        }
        let mut v = Vec::new();
        for line in String::from_utf8_lossy(&o.stdout).lines().skip(1) {
            let c: Vec<&str> = line.split_whitespace().collect();
            if c.len() < 10 {
                continue;
            }
            let (start, end, prt, res, tp) = (c[1], c[2], c[3], c[4], c[9]);
            if !prt.starts_with("rw") || !(tp == "sw" || tp == "df") {
                continue;
            }
            let hex = |s: &str| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok();
            let (Some(start), Some(end), Ok(res)) = (hex(start), hex(end), res.parse::<u64>()) else { continue };
            v.push(VmRegion { r: Region { start, end }, resident_pages: res });
        }
        Ok(v)
    }

    /// PT_ATTACH stops the target; this guard always detaches it again.
    struct Attached(libc::pid_t);
    impl Drop for Attached {
        fn drop(&mut self) {
            unsafe {
                libc::ptrace(libc::PT_DETACH, self.0, 1 as *mut libc::c_char, 0);
            }
        }
    }

    fn attach(pid: i32) -> io::Result<Attached> {
        if unsafe { libc::ptrace(libc::PT_ATTACH, pid, std::ptr::null_mut(), 0) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let g = Attached(pid);
        let mut status = 0;
        if unsafe { libc::waitpid(pid, &mut status, 0) } != pid {
            return Err(io::Error::last_os_error());
        }
        Ok(g)
    }

    fn read_page(pid: i32, addr: u64, buf: &mut [u8]) -> bool {
        let mut d = libc::ptrace_io_desc {
            piod_op: libc::PIOD_READ_D,
            piod_offs: addr as *mut libc::c_void,
            piod_addr: buf.as_mut_ptr() as *mut libc::c_void,
            piod_len: buf.len(),
        };
        let r = unsafe { libc::ptrace(libc::PT_IO, pid, &mut d as *mut _ as *mut libc::c_char, 0) };
        r == 0 && d.piod_len == buf.len()
    }

    pub fn for_each_anon_page(pid: i32, opts: &ReadOpts, f: &mut dyn FnMut(&[u8])) -> io::Result<Coverage> {
        if pid == std::process::id() as i32 {
            return Err(io::Error::other("refusing to probe self"));
        }
        let regions = regions(pid)?;
        let _g = attach(pid)?;
        let sample = opts.sample.max(1);
        let mut cov = Coverage::default();
        let mut buf = vec![0u8; PAGE as usize];
        let mut idx: u64 = 0;
        for vr in regions {
            let n = (vr.r.end - vr.r.start) / PAGE;
            if vr.resident_pages == 0 {
                continue;
            }
            if vr.resident_pages < n {
                if !opts.fault_partial {
                    cov.skipped += vr.resident_pages * PAGE;
                    continue;
                }
                cov.faulted += (n - vr.resident_pages) * PAGE / sample;
            }
            for i in 0..n {
                idx += 1;
                if idx % sample != 0 {
                    continue;
                }
                if read_page(pid, vr.r.start + i * PAGE, &mut buf) {
                    cov.read += PAGE;
                    f(&buf);
                }
            }
        }
        Ok(cov)
    }
}

#[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
mod imp {
    use super::*;
    pub fn list_procs() -> Vec<(i32, String)> {
        Vec::new()
    }
    pub fn for_each_anon_page(_: i32, _: &ReadOpts, _: &mut dyn FnMut(&[u8])) -> io::Result<Coverage> {
        Err(io::Error::other("memprobe snapshot supports Linux and FreeBSD only"))
    }
}
