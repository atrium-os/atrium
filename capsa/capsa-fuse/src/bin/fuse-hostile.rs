//! atrium-fuse-hostile: a hostile FUSE server, to fuzz the kernel's fusefs
//! from the server side (network-shares.md §4.4, item 1). A share server
//! that a remote attacker has taken over can send the kernel anything over
//! `/dev/fuse`; this one does, on purpose, while driver processes use the
//! mount as an ordinary user would.
//!
//!   atrium-fuse-hostile --mnt DIR --secs N [--seed S] [--uid U] [--drivers K]
//!                       [--mutate PERCENT] [--timeout SECS]
//!
//! Run as root on FreeBSD (it mounts fusefs itself, with the options jaild
//! uses: nosuid, noexec, allow_other, default_permissions, timeout=). It
//! answers most requests plausibly from a small made-up tree, and mutates
//! the rest: bodies cut short or padded, lengths that disagree with what is
//! written, odd error values (ERESTART, EJUSTRETURN, positive, i32::MIN),
//! wrong or repeated uniques, absurd attributes (types that change, inode 0,
//! sizes and validity at u64::MAX), directory entries with bad lengths and
//! names, reads longer than asked, unsolicited notifications, replies
//! dropped and sent late. The kernel must survive all of it and the mount
//! must still unmount. A run is reproducible from its seed.
//!
//! Prints `fuse-hostile: seed S: R requests, M mutated, N notifies, D dropped`
//! and exits 0 if the mount came back down.

#[cfg(not(target_os = "freebsd"))]
fn main() {
    eprintln!("fuse-hostile: FreeBSD only (it mounts fusefs)");
    std::process::exit(2);
}

#[cfg(target_os = "freebsd")]
fn main() {
    std::process::exit(freebsd::run());
}

/// xorshift64*: small, seedable, good enough to steer a fuzzer.
#[derive(Clone)]
#[allow(dead_code)]
struct Rng(u64);

#[allow(dead_code)]
impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed.max(1))
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next() % n }
    }
    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
    fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next() as u8).collect()
    }
    fn some(&mut self, max: u64) -> Vec<u8> {
        let n = self.below(max) as usize;
        self.bytes(n)
    }
    fn pick<'a, T>(&mut self, v: &'a [T]) -> &'a T {
        &v[self.below(v.len() as u64) as usize]
    }
}

#[cfg(target_os = "freebsd")]
mod freebsd {
    use super::Rng;
    use capsa_fuse::proto::{self, FAttr, Out};
    use std::ffi::CString;
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    use std::time::{Duration, Instant};

    const NAMES: [&str; 10] = ["a", "b", "c.txt", "dir1", "dir2", "lnk", "x", "dir3", "big.bin", "y"];
    const S_IFDIR: u32 = 0o040000;
    const S_IFREG: u32 = 0o100000;
    const S_IFLNK: u32 = 0o120000;

    // Opcodes beyond capsa's list.
    const READLINK: u32 = 5;
    const SYMLINK: u32 = 6;
    const MKNOD: u32 = 8;
    const LINK: u32 = 13;
    const GETXATTR: u32 = 22;
    const LISTXATTR: u32 = 23;
    const GETLK: u32 = 31;
    const BMAP: u32 = 37;
    const FALLOCATE: u32 = 43;
    const LSEEK: u32 = 46;
    const COPY_FILE_RANGE: u32 = 47;

    // Notify codes (the error field of an unsolicited message, unique 0).
    const NOTIFY_INVAL_INODE: i32 = 2;
    const NOTIFY_INVAL_ENTRY: i32 = 3;
    const NOTIFY_STORE: i32 = 4;
    const NOTIFY_RETRIEVE: i32 = 5;
    const NOTIFY_DELETE: i32 = 6;

    struct Args {
        mnt: String,
        secs: u64,
        seed: u64,
        uid: u32,
        drivers: u32,
        mutate: u64,
        timeout: u32,
        /// A named, deterministic case instead of the fuzz loop.
        repro: Option<String>,
    }

    fn args() -> Result<Args, String> {
        let mut a = Args { mnt: String::new(), secs: 60, seed: 0, uid: 65534, drivers: 3, mutate: 30, timeout: 5, repro: None };
        let v: Vec<String> = std::env::args().skip(1).collect();
        let mut i = 0;
        while i < v.len() {
            let val = v.get(i + 1).cloned().ok_or_else(|| format!("{} needs a value", v[i]))?;
            let num = || val.parse::<u64>().map_err(|_| format!("bad number {val}"));
            match v[i].as_str() {
                "--mnt" => a.mnt = val.clone(),
                "--secs" => a.secs = num()?,
                "--seed" => a.seed = num()?,
                "--uid" => a.uid = num()? as u32,
                "--drivers" => a.drivers = num()?.clamp(1, 16) as u32,
                "--mutate" => a.mutate = num()?.min(100),
                "--timeout" => a.timeout = num()?.clamp(1, 600) as u32,
                "--repro" => a.repro = Some(val.clone()),
                o => return Err(format!("unknown argument {o}")),
            }
            i += 2;
        }
        if a.mnt.is_empty() {
            return Err("--mnt is required".into());
        }
        if a.seed == 0 {
            a.seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(1);
        }
        Ok(a)
    }

    /// The same nmount(2) jaild makes (portcullis/jaild/src/ffi.rs).
    fn mount(target: &str, fd: i32, timeout: u32) -> std::io::Result<()> {
        let opts: Vec<(&str, Option<String>)> = vec![
            ("fstype", Some("fusefs".into())),
            ("fspath", Some(target.into())),
            ("from", Some("/dev/fuse".into())),
            ("fd", Some(fd.to_string())),
            ("fsname", Some("fuse-hostile".into())),
            ("allow_other", None),
            ("default_permissions", None),
            ("timeout=", Some(timeout.to_string())),
        ];
        let mut strs = Vec::new();
        for (k, v) in &opts {
            strs.push(CString::new(*k).unwrap());
            if let Some(v) = v {
                strs.push(CString::new(v.as_str()).unwrap());
            }
        }
        let mut iov = Vec::new();
        let mut it = strs.iter();
        for (_, v) in &opts {
            let k = it.next().unwrap();
            iov.push(libc::iovec { iov_base: k.as_ptr() as *mut _, iov_len: k.as_bytes_with_nul().len() });
            match v {
                Some(_) => {
                    let c = it.next().unwrap();
                    iov.push(libc::iovec { iov_base: c.as_ptr() as *mut _, iov_len: c.as_bytes_with_nul().len() });
                }
                None => iov.push(libc::iovec { iov_base: std::ptr::null_mut(), iov_len: 0 }),
            }
        }
        let flags = libc::MNT_NOSUID | libc::MNT_NOEXEC;
        // SAFETY: every iov_base points into a CString in `strs`.
        let rc = unsafe { libc::nmount(iov.as_mut_ptr(), iov.len() as u32, flags as i32) };
        if rc < 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
    }

    /// What a node is, from its id: the made-up tree every honest answer
    /// comes from. 1 is the root.
    fn kind_of(ino: u64) -> u32 {
        if ino == 1 { return S_IFDIR; }
        match NAMES[(ino.wrapping_sub(2) as usize) % NAMES.len()] {
            n if n.starts_with("dir") => S_IFDIR,
            "lnk" => S_IFLNK,
            _ => S_IFREG,
        }
    }

    fn ino_for(parent: u64, name: &str) -> u64 {
        let i = NAMES.iter().position(|n| *n == name).unwrap_or(0) as u64;
        // Distinct per (parent, name), and kind_of agrees with the name.
        2 + i + NAMES.len() as u64 * (parent % 7)
    }

    fn attr(ino: u64, uid: u32) -> FAttr {
        let kind = kind_of(ino);
        let size = match kind { S_IFDIR => 512, S_IFLNK => 5, _ => 4096 + (ino % 5) * 1000 };
        FAttr { ino, size, blocks: size.div_ceil(512), atime: 1_700_000_000, mtime: 1_700_000_000, ctime: 1_700_000_000,
            mode: kind | if kind == S_IFDIR { 0o755 } else { 0o644 }, nlink: if kind == S_IFDIR { 2 } else { 1 }, uid, gid: 0, blksize: 4096 }
    }

    fn evil_attr(r: &mut Rng, ino: u64, uid: u32) -> FAttr {
        let mut a = attr(ino, uid);
        match r.below(9) {
            0 => a.ino = 0,
            1 => a.ino = 1,
            2 => a.size = u64::MAX,
            3 => a.size = r.next(),
            4 => a.mode = r.next() as u32,
            5 => a.mode = (a.mode & 0o7777) | *r.pick(&[S_IFDIR, S_IFREG, S_IFLNK, 0o020000, 0o060000, 0o140000, 0o010000, 0]),
            6 => a.nlink = *r.pick(&[0, u32::MAX]),
            7 => a.blocks = u64::MAX,
            _ => { a.atime = i64::MIN; a.mtime = -1; a.ctime = i64::MAX; }
        }
        a
    }

    fn entry(a: &FAttr, valid: u64) -> Vec<u8> {
        proto::entry_out(a, valid)
    }

    fn open_out(fh: u64) -> Vec<u8> {
        let mut o = Out::new();
        o.u64(fh).u32(0).u32(0);
        o.0
    }

    fn statfs_out(r: &mut Rng, evil: bool) -> Vec<u8> {
        let mut o = Out::new();
        if evil {
            for _ in 0..5 { o.u64(r.next()); }
            o.u32(*r.pick(&[0, 1, u32::MAX, 3])).u32(*r.pick(&[0, 255, u32::MAX])).u32(*r.pick(&[0, 512, u32::MAX])).u32(r.next() as u32);
        } else {
            o.u64(1 << 20).u64(1 << 19).u64(1 << 19).u64(1000).u64(500);
            o.u32(4096).u32(255).u32(4096).u32(0);
        }
        for _ in 0..6 { o.u32(0); }
        o.0
    }

    fn readdir(r: &mut Rng, parent: u64, size: usize, offset: u64, evil: bool) -> Vec<u8> {
        let mut o = Out::new();
        if offset as usize >= NAMES.len() + 2 && !evil {
            return o.0;
        }
        let mut i = offset;
        while (i as usize) < NAMES.len() + 2 {
            let (name, ino): (String, u64) = match i {
                0 => (".".into(), parent),
                1 => ("..".into(), 1),
                n => { let nm = NAMES[(n - 2) as usize]; (nm.into(), ino_for(parent, nm)) }
            };
            if evil && r.chance(40) {
                // A bad entry: lengths that lie, names that can't be names.
                let bad: Vec<u8> = match r.below(6) {
                    0 => b"a/b".to_vec(),
                    1 => b"x\0y".to_vec(),
                    2 => vec![b'z'; 300],
                    3 => Vec::new(),
                    4 => { let n = 1 + r.below(40) as usize; r.bytes(n) },
                    _ => b"..".to_vec(),
                };
                let namelen = if r.chance(50) { bad.len() as u32 } else { r.next() as u32 };
                let off = if r.chance(50) { 0 } else { r.next() };
                o.u64(if r.chance(30) { 0 } else { ino }).u64(off).u32(namelen).u32(r.next() as u32 % 16).bytes(&bad);
                while o.0.len() % 8 != 0 { o.0.push(0); }
            } else {
                proto::dirent(&mut o, ino, i + 1, (kind_of(ino) >> 12) & 0o17, &name);
            }
            if o.0.len() > size && !evil {
                o.0.truncate(o.0.len() - proto::dirent_len(&name));
                break;
            }
            i += 1;
        }
        o.0
    }

    /// The honest answer, or None for no reply at all.
    fn honest(req: &proto::Request, uid: u32) -> Option<Result<Vec<u8>, i32>> {
        let b = req.body;
        let size_at = |o: usize| proto::u32le(b, o).unwrap_or(0) as usize;
        Some(match req.opcode {
            proto::FORGET | proto::BATCH_FORGET | proto::INTERRUPT => return None,
            proto::LOOKUP => match proto::cstr(b) {
                Some((name, _)) if NAMES.contains(&name) && kind_of(req.nodeid) == S_IFDIR => {
                    Ok(entry(&attr(ino_for(req.nodeid, name), uid), 1))
                }
                _ => Err(libc::ENOENT),
            },
            proto::GETATTR | proto::SETATTR => Ok(proto::attr_out(&attr(req.nodeid, uid), 1)),
            READLINK => Ok(b"c.txt".to_vec()),
            proto::MKDIR | SYMLINK | MKNOD | LINK => Ok(entry(&attr(ino_for(req.nodeid, "dir3"), uid), 1)),
            proto::CREATE => {
                let mut v = entry(&attr(ino_for(req.nodeid, "x"), uid), 1);
                v.extend(open_out(7));
                Ok(v)
            }
            proto::OPEN | proto::OPENDIR => Ok(open_out(req.nodeid)),
            proto::READ => {
                let off = proto::u64le(b, 8).unwrap_or(0) as usize;
                let len = attr(req.nodeid, uid).size as usize;
                let n = size_at(16).min(len.saturating_sub(off));
                Ok((0..n).map(|i| b'a' + ((off + i) % 26) as u8).collect())
            }
            proto::WRITE => {
                let mut o = Out::new();
                o.u32(size_at(16) as u32).u32(0);
                Ok(o.0)
            }
            proto::STATFS => Ok(statfs_out(&mut Rng::new(1), false)),
            proto::READDIR => {
                let off = proto::u64le(b, 8).unwrap_or(0);
                Ok(readdir(&mut Rng::new(1), req.nodeid, size_at(16), off, false))
            }
            proto::RELEASE | proto::RELEASEDIR | proto::FLUSH | proto::FSYNC | proto::FSYNCDIR | proto::UNLINK
            | proto::RMDIR | proto::RENAME | proto::RENAME2 | proto::ACCESS => Ok(Vec::new()),
            proto::DESTROY => Ok(Vec::new()),
            _ => Err(libc::ENOSYS),
        })
    }

    /// A mutated answer for this request.
    fn evil(r: &mut Rng, req: &proto::Request, uid: u32, good: &Result<Vec<u8>, i32>) -> (i32, Vec<u8>) {
        let b = req.body;
        let size_at = |o: usize| proto::u32le(b, o).unwrap_or(0) as usize;
        let goodbody = good.clone().unwrap_or_default();
        match r.below(12) {
            0 => (0, r.some(300)),
            1 => { let mut v = goodbody; let k = r.below(v.len() as u64 + 1) as usize; v.truncate(v.len() - k); (0, v) }
            2 => { let mut v = goodbody; let n = r.below(200) as usize; v.extend(r.bytes(n)); (0, v) }
            // An error with a body is a format error that ends the session,
            // so mostly send none: then the kernel has to judge the value.
            3 => { let e = *r.pick(&[1, 2, 3, -1, -4, -85, -86, i32::MAX, i32::MIN, 1000, -1000, -4096]); (e, if r.chance(10) { r.some(64) } else { Vec::new() }) }
            4 => (0, match req.opcode {
                proto::LOOKUP | proto::MKDIR | proto::CREATE | SYMLINK | MKNOD | LINK => {
                    let mut v = entry(&evil_attr(r, ino_for(req.nodeid, "a"), uid), *r.pick(&[0, 1, u64::MAX, u64::MAX / 2]));
                    if r.chance(30) { v[0..8].copy_from_slice(&r.pick(&[0u64, 1, req.nodeid, u64::MAX]).to_le_bytes()); }
                    if req.opcode == proto::CREATE { v.extend(open_out(r.next())); }
                    v
                }
                _ => proto::attr_out(&evil_attr(r, req.nodeid, uid), *r.pick(&[0, 1, u64::MAX])),
            }),
            5 => (0, match req.opcode {
                proto::READDIR => readdir(r, req.nodeid, size_at(16), proto::u64le(b, 8).unwrap_or(0), true),
                proto::READ => { let n = size_at(16) + 1 + r.below(8192) as usize; r.bytes(n) },
                proto::WRITE => { let mut o = Out::new(); o.u32(*r.pick(&[u32::MAX, size_at(16) as u32 + 1, 0])).u32(r.next() as u32); o.0 }
                proto::STATFS => statfs_out(r, true),
                READLINK => match r.below(4) { 0 => Vec::new(), 1 => vec![b'/'; 2000], 2 => b"../../../../etc/passwd".to_vec(), _ => r.bytes(64) },
                proto::OPEN | proto::OPENDIR | proto::CREATE => { let mut o = Out::new(); o.u64(r.next()).u32(r.next() as u32).u32(r.next() as u32); o.0 }
                GETXATTR | LISTXATTR | LSEEK | GETLK | BMAP | FALLOCATE | COPY_FILE_RANGE => r.some(40),
                _ => r.some(128),
            }),
            // 6..: handled by the caller (unique, length, drop, duplicate, delay)
            n => (n as i32 * -1000, goodbody),
        }
    }

    /// A reply with `error` written as is (proto::reply negates an errno).
    fn frame(unique: u64, error: i32, body: &[u8]) -> Vec<u8> {
        let mut v = Vec::with_capacity(proto::OUT_HEADER + body.len());
        v.extend_from_slice(&((proto::OUT_HEADER + body.len()) as u32).to_le_bytes());
        v.extend_from_slice(&error.to_le_bytes());
        v.extend_from_slice(&unique.to_le_bytes());
        v.extend_from_slice(body);
        v
    }

    fn notify(r: &mut Rng) -> Vec<u8> {
        let code = *r.pick(&[NOTIFY_INVAL_INODE, NOTIFY_INVAL_ENTRY, NOTIFY_STORE, NOTIFY_RETRIEVE, NOTIFY_DELETE, 0, 1, 7, 99]);
        let mut o = Out::new();
        let ino = *r.pick(&[1u64, 2, 5, 0, u64::MAX, ino_for(1, "dir1")]);
        match code {
            NOTIFY_INVAL_INODE => { o.u64(ino).u64(r.next()).u64(r.next()); }
            NOTIFY_INVAL_ENTRY | NOTIFY_DELETE => {
                let name: &[u8] = match r.below(4) { 0 => b"a", 1 => b"dir1", 2 => b"..", _ => b"x/y" };
                o.u64(ino);
                if code == NOTIFY_DELETE { o.u64(ino_for(1, "a")); }
                o.u32(if r.chance(70) { name.len() as u32 } else { r.next() as u32 }).u32(0).bytes(name);
                if r.chance(70) { o.0.push(0); }
            }
            NOTIFY_STORE => { let n = r.below(9000) as u32; o.u64(ino).u64(r.next() % 100_000).u32(if r.chance(70) { n } else { r.next() as u32 }).u32(0); let d = r.bytes(n as usize); o.bytes(&d); }
            NOTIFY_RETRIEVE => { o.u64(r.next()).u64(ino).u64(r.next() % 100_000).u32(r.next() as u32 % 70_000).u32(0).u64(0); }
            _ => { let n = r.below(64) as usize; o.bytes(&r.bytes(n)); }
        }
        // A notify is unique 0 with the code in the error field.
        let mut v = Vec::new();
        v.extend_from_slice(&((proto::OUT_HEADER + o.0.len()) as u32).to_le_bytes());
        v.extend_from_slice(&code.to_le_bytes());
        v.extend_from_slice(&0u64.to_le_bytes());
        v.extend(o.0);
        v
    }

    fn init_reply(body: &[u8]) -> Vec<u8> {
        let minor = proto::u32le(body, 4).unwrap_or(0).min(proto::MINOR);
        let readahead = proto::u32le(body, 8).unwrap_or(0);
        let mut o = Out::new();
        o.u32(proto::MAJOR).u32(minor).u32(readahead).u32(0);
        o.u16(16).u16(12).u32(128 * 1024);
        o.u32(1).u16(0).u16(0).u32(0);
        for _ in 0..7 { o.u32(0); }
        if minor < 23 { o.0.truncate(24); }
        o.0
    }

    /// A user hammering the mount: every file call a program might make.
    fn drive(mnt: &str, seed: u64, until: Instant) {
        let mut r = Rng::new(seed);
        let path = |r: &mut Rng| -> String {
            let a = *r.pick(&NAMES);
            match r.below(3) { 0 => format!("{mnt}/{a}"), 1 => format!("{mnt}/dir1/{a}"), _ => format!("{mnt}/dir2/dir1/{a}") }
        };
        let mut buf = vec![0u8; 70_000];
        while Instant::now() < until {
            let p = path(&mut r);
            let q = path(&mut r);
            match r.below(18) {
                0 => { let _ = std::fs::metadata(&p); }
                1 => { let _ = std::fs::symlink_metadata(&p); }
                2 => { if let Ok(d) = std::fs::read_dir(format!("{mnt}/{}", r.pick(&["", "dir1", "dir2", "a"]))) { for e in d.take(64) { let _ = e.map(|e| e.file_type()); } } }
                3 => { if let Ok(mut f) = std::fs::File::open(&p) { let n = r.below(buf.len() as u64) as usize; let _ = f.read(&mut buf[..n]); } }
                4 => { if let Ok(mut f) = std::fs::OpenOptions::new().write(true).open(&p) { let n = r.below(9000) as usize; let _ = f.write(&buf[..n]); let _ = f.sync_all(); } }
                5 => { let _ = std::fs::File::create(&p); }
                6 => { let _ = std::fs::create_dir(&p); }
                7 => { let _ = std::fs::remove_file(&p); }
                8 => { let _ = std::fs::remove_dir(&p); }
                9 => { let _ = std::fs::rename(&p, &q); }
                10 => { let _ = std::fs::read_link(&p); }
                11 => { let _ = std::os::unix::fs::symlink("c.txt", &p); }
                12 => { let _ = std::fs::hard_link(&p, &q); }
                13 => { if let Ok(f) = std::fs::OpenOptions::new().write(true).open(&p) { let _ = f.set_len(r.below(1 << 20)); } }
                14 => {
                    if let Ok(f) = std::fs::File::open(&p) {
                        // SAFETY: a private read-only mapping of a file we opened; touched, then unmapped.
                        unsafe {
                            let len = 16384usize;
                            let m = libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ, libc::MAP_PRIVATE, f.as_raw_fd(), 0);
                            if m != libc::MAP_FAILED {
                                // Touching past EOF is SIGBUS: only the first byte.
                                std::hint::black_box(std::ptr::read_volatile(m as *const u8));
                                libc::munmap(m, len);
                            }
                        }
                    }
                }
                15 => {
                    let c = CString::new(p.clone()).unwrap();
                    // SAFETY: statfs into a zeroed struct we own.
                    unsafe { let mut s: libc::statfs = std::mem::zeroed(); libc::statfs(c.as_ptr(), &mut s); }
                }
                16 => {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(r.below(0o7777) as u32));
                }
                _ => {
                    if let Ok(mut f) = std::fs::File::open(&p) {
                        use std::io::Seek;
                        let _ = f.seek(std::io::SeekFrom::Start(r.next() % (1 << 30)));
                        let _ = f.read(&mut buf[..512]);
                    }
                }
            }
        }
    }

    pub fn run() -> i32 {
        let a = match args() {
            Ok(a) => a,
            Err(e) => { eprintln!("fuse-hostile: {e}"); return 2; }
        };
        let _ = std::fs::create_dir_all(&a.mnt);
        let dev = match std::fs::OpenOptions::new().read(true).write(true).open("/dev/fuse") {
            Ok(f) => f,
            Err(e) => { eprintln!("fuse-hostile: /dev/fuse: {e}"); return 1; }
        };
        if let Err(e) = mount(&a.mnt, dev.as_raw_fd(), a.timeout) {
            eprintln!("fuse-hostile: mount {}: {e}", a.mnt);
            return 1;
        }
        eprintln!("fuse-hostile: seed {} on {} for {} s, {} drivers as uid {}, {}% mutated, timeout {} s",
            a.seed, a.mnt, a.secs, a.drivers, a.uid, a.mutate, a.timeout);
        let until = Instant::now() + Duration::from_secs(a.secs);
        // The drivers: forked, each its own user process, so a request the
        // kernel holds blocks one of them, not the server.
        let mut kids = Vec::new();
        for k in 0..a.drivers {
            // SAFETY: fork in a process with no other threads yet.
            let pid = unsafe { libc::fork() };
            if pid == 0 {
                // SAFETY: dropping to the driver's uid in the child.
                unsafe { libc::setgid(a.uid); libc::setuid(a.uid); }
                // SAFETY: the child doesn't serve; close its copy of the device.
                unsafe { libc::close(dev.as_raw_fd()); }
                if a.repro.as_deref() == Some("readlink-kill") {
                    // Walk through the symlink: namei reads it, and the
                    // server never answers that READLINK.
                    let p = format!("{}/lnk/x", a.mnt);
                    while Instant::now() < until { let _ = std::fs::metadata(&p); }
                } else if a.repro.as_deref() == Some("fsync-timeout") {
                    // Two handles on one file (read-only and write-only
                    // opens), then fsync: the kernel sends one FSYNC per
                    // handle, reusing its ticket after the first one timed
                    // out (the server never answers FSYNC).
                    let p = format!("{}/a", a.mnt);
                    let r = std::fs::File::open(&p);
                    let w = std::fs::OpenOptions::new().write(true).open(&p);
                    match (&r, &w) {
                        (Ok(_), Ok(w)) => eprintln!("fuse-hostile: repro: fsync with two handles: {:?}", w.sync_all()),
                        _ => eprintln!("fuse-hostile: repro: open failed: {:?} {:?}", r.err(), w.err()),
                    }
                    while Instant::now() < until { std::thread::sleep(Duration::from_millis(100)); }
                } else if a.repro.as_deref() == Some("dir-cycle") {
                    // One driver walks down through "up" (which the server
                    // says is the root) while the other walks root -> dir1:
                    // with the cycle accepted, they take the same two vnode
                    // locks in opposite orders.
                    let p = if k == 0 { format!("{}/dir1/up/dir1/up/dir1/a", a.mnt) } else { format!("{}/dir1/a", a.mnt) };
                    if k == 0 {
                        match std::fs::metadata(format!("{}/dir1/up", a.mnt)) {
                            Ok(_) => eprintln!("fuse-hostile: repro: dir1/up accepted: the kernel took the root as dir1's child"),
                            Err(e) => eprintln!("fuse-hostile: repro: dir1/up refused: {e}"),
                        }
                    }
                    while Instant::now() < until { let _ = std::fs::metadata(&p); }
                } else {
                    drive(&a.mnt, a.seed ^ (k as u64 + 1).wrapping_mul(0x9e37_79b9_7f4a_7c15), until);
                }
                // SAFETY: leave without running the parent's destructors.
                unsafe { libc::_exit(0) };
            }
            kids.push(pid);
        }
        let mut r = Rng::new(a.seed);
        let mut dev = dev;
        let (mut reqs, mut muts, mut notes, mut drops, mut killed) = (0u64, 0u64, 0u64, 0u64, 0u64);
        let mut held: Vec<(u64, Instant)> = Vec::new();
        let mut buf = vec![0u8; 256 * 1024];
        let mut started = false;
        let stop_at = until;
        let mut unmounter: Option<std::thread::JoinHandle<i32>> = None;
        let mut honest_only = false;
        let mut stop_at = stop_at;
        let mut parked: Vec<u64> = Vec::new();
        loop {
            // Time's up: drivers go, then unmount from another thread while
            // this one keeps answering (honestly) until the kernel lets go.
            // SIGKILL wakes a driver sleeping on an answer and fails its
            // syscall with ERESTART: on an INVARIANTS kernel a READLINK cut
            // short like that inside namei() panics (--repro readlink-kill).
            // The fuzz rounds stop drivers with SIGTERM instead, which
            // fusefs holds until the answer, so they keep finding new bugs.
            if !honest_only && Instant::now() >= stop_at {
                let sig = if a.repro.is_some() { libc::SIGKILL } else { libc::SIGTERM };
                for p in &kids {
                    // SAFETY: signalling our own children.
                    unsafe { libc::kill(*p, sig); }
                }
                honest_only = true;
                for u in parked.drain(..) {
                    let _ = dev.write(&frame(u, 0, b"c.txt"));
                }
            }
            // SAFETY: reaping our own children without blocking: one may
            // still wait for an answer only this loop can give.
            kids.retain(|p| unsafe { let mut st = 0; libc::waitpid(*p, &mut st, libc::WNOHANG) == 0 });
            if unmounter.is_none() && honest_only && kids.is_empty() {
                let m = a.mnt.clone();
                unmounter = Some(std::thread::spawn(move || {
                    let c = CString::new(m).unwrap();
                    // SAFETY: unmounting the mount we made.
                    let rc = unsafe { libc::unmount(c.as_ptr(), 0) };
                    if rc != 0 { unsafe { libc::unmount(c.as_ptr(), libc::MNT_FORCE) } } else { 0 }
                }));
            }
            if unmounter.as_ref().is_some_and(|t| t.is_finished()) {
                break;
            }
            let mut pfd = libc::pollfd { fd: dev.as_raw_fd(), events: libc::POLLIN, revents: 0 };
            // SAFETY: one pollfd we own.
            let ready = unsafe { libc::poll(&mut pfd, 1, 20) };
            // Late replies to requests we sat on (the kernel may have given up).
            let now = Instant::now();
            for (u, _) in held.iter().filter(|(_, t)| *t <= now) {
                let junk = r.some(128);
                let _ = dev.write(&frame(*u, 0, &junk));
            }
            held.retain(|(_, t)| *t > now);
            if started && !honest_only && r.chance(1) && a.mutate > 0 {
                notes += 1;
                let _ = dev.write(&notify(&mut r));
            }
            if ready <= 0 || pfd.revents & (libc::POLLIN | libc::POLLERR | libc::POLLHUP) == 0 {
                continue;
            }
            let n = match dev.read(&mut buf) {
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(e) if e.raw_os_error() == Some(libc::ENODEV) && honest_only => break, // unmounted
                Err(e) if e.raw_os_error() == Some(libc::ENODEV) => {
                    // The kernel ended the session over a malformed reply,
                    // as it should. Mount again and carry on with the round.
                    killed += 1;
                    let c = CString::new(a.mnt.clone()).unwrap();
                    // SAFETY: unmounting the mount we made.
                    unsafe { libc::unmount(c.as_ptr(), libc::MNT_FORCE) };
                    let fresh = std::fs::OpenOptions::new().read(true).write(true).open("/dev/fuse");
                    match fresh {
                        Ok(f) if mount(&a.mnt, f.as_raw_fd(), a.timeout).is_ok() => { dev = f; held.clear(); parked.clear(); started = false; continue }
                        _ => { eprintln!("fuse-hostile: could not mount again"); break }
                    }
                }
                Err(e) if e.raw_os_error() == Some(libc::EINTR) => continue,
                Err(e) => { eprintln!("fuse-hostile: read: {e}"); break; }
            };
            let Some(req) = proto::parse(&buf[..n]) else {
                eprintln!("fuse-hostile: the kernel sent a request that doesn't frame ({n} bytes)");
                continue;
            };
            reqs += 1;
            if req.opcode == proto::INIT {
                started = true;
                let _ = dev.write(&frame(req.unique, 0, &init_reply(req.body)));
                continue;
            }
            if a.repro.as_deref() == Some("readlink-kill") && req.opcode == READLINK && !honest_only {
                // The driver now sleeps in the kernel waiting for this
                // answer; SIGKILL it there (the stop path below), then
                // answer honestly so the unmount can finish.
                eprintln!("fuse-hostile: repro: holding READLINK {}, killing the driver", req.unique);
                parked.push(req.unique);
                stop_at = Instant::now();
                continue;
            }
            if a.repro.as_deref() == Some("fsync-timeout") && req.opcode == proto::FSYNC && !honest_only {
                drops += 1;
                continue; // never answered: the kernel times out
            }
            if a.repro.as_deref() == Some("dir-cycle") && req.opcode == proto::LOOKUP && proto::cstr(req.body).is_some_and(|(n, _)| n == "up") {
                let _ = dev.write(&frame(req.unique, 0, &entry(&attr(1, a.uid), 0)));
                continue;
            }
            let Some(good) = honest(&req, a.uid) else { continue };
            if req.opcode == proto::DESTROY || honest_only || !r.chance(a.mutate) {
                let out = match &good { Ok(b) => frame(req.unique, 0, b), Err(e) => frame(req.unique, -*e, &[]) };
                let _ = dev.write(&out);
                continue;
            }
            muts += 1;
            let (err, body) = evil(&mut r, &req, a.uid, &good);
            let out = match err {
                -6000 => frame(r.next(), 0, &body),                         // a unique the kernel never sent
                // A length that lies ends the session (fuse_ohead_audit): rare,
                // or the rest of the round tests nothing.
                -7000 if r.chance(10) => { let mut v = frame(req.unique, 0, &body); let l = r.next() as u32 % 512; v[0..4].copy_from_slice(&l.to_le_bytes()); v }
                -7000 => frame(req.unique, 0, &body[..r.below(body.len() as u64 + 1) as usize]),
                -8000 => { drops += 1; held.push((req.unique, Instant::now() + Duration::from_millis(r.below(a.timeout as u64 * 1500)))); continue }
                -9000 => { let v = frame(req.unique, 0, &body); let _ = dev.write(&v); v } // twice
                -10000 => { std::thread::sleep(Duration::from_millis(r.below(a.timeout as u64 * 300))); frame(req.unique, 0, &body) }
                -11000 => frame(req.unique, 0, &body[..body.len().min(r.below(17) as usize)]),
                e => frame(req.unique, e, &body),
            };
            let _ = dev.write(&out);
        }
        let rc = match unmounter {
            Some(t) => t.join().unwrap_or(-1),
            None => {
                let c = CString::new(a.mnt.clone()).unwrap();
                // SAFETY: unmounting the mount we made (the device is gone).
                unsafe { libc::unmount(c.as_ptr(), libc::MNT_FORCE) }
            }
        };
        drop(dev);
        println!("fuse-hostile: seed {}: {reqs} requests, {muts} mutated, {notes} notifies, {drops} dropped, {killed} sessions ended by the kernel; unmount {}",
            a.seed, if rc == 0 { "ok" } else { "FAILED" });
        if rc == 0 { 0 } else { 1 }
    }
}
