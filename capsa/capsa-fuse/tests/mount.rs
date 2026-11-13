//! The server under a real kernel FUSE mount (Linux, as root; FreeBSD's
//! fusefs gets the same protocol, checked on the VM). Skipped, loudly,
//! where mounting is not possible.

#![cfg(target_os = "linux")]

use std::ffi::CString;
use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread::JoinHandle;

use capsa_fat::Volume;
use capsa_fuse::{serve, Options};

struct Mount {
    dir: PathBuf,
    th: Option<JoinHandle<std::io::Result<()>>>,
}

impl Mount {
    /// Mount `fs` at `dir` the way jaild will: open /dev/fuse, mount(2)
    /// with that fd, hand the fd to the server.
    fn new<B: capsa_fuse::Backend + Send + 'static>(fs: B, dir: &Path, opts: Options) -> Option<Mount> {
        let f = std::fs::OpenOptions::new().read(true).write(true).open("/dev/fuse").ok()?;
        let src = CString::new("capsa").unwrap();
        let tgt = CString::new(dir.as_os_str().as_encoded_bytes()).unwrap();
        let ty = CString::new("fuse.capsa").unwrap();
        let data = CString::new(format!("fd={},rootmode=40000,user_id=0,group_id=0,default_permissions,allow_other", f.as_raw_fd())).unwrap();
        // SAFETY: valid C strings; failure is reported and the test skips.
        let r = unsafe { libc::mount(src.as_ptr(), tgt.as_ptr(), ty.as_ptr(), libc::MS_NOSUID | libc::MS_NODEV, data.as_ptr().cast()) };
        if r != 0 {
            eprintln!("SKIP: mount(2) fuse: {}", std::io::Error::last_os_error());
            return None;
        }
        let th = std::thread::spawn(move || serve(fs, f, opts));
        Some(Mount { dir: dir.to_path_buf(), th: Some(th) })
    }

    fn unmount(mut self) -> std::io::Result<()> {
        let t = CString::new(self.dir.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: a valid path.
        unsafe { libc::umount2(t.as_ptr(), 0) };
        self.th.take().unwrap().join().unwrap()
    }
}

impl Drop for Mount {
    fn drop(&mut self) {
        if self.th.is_some() {
            let t = CString::new(self.dir.as_os_str().as_encoded_bytes()).unwrap();
            // SAFETY: a valid path.
            unsafe { libc::umount2(t.as_ptr(), libc::MNT_DETACH) };
        }
    }
}

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("capsa-fuse-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("mnt")).unwrap();
    d
}

fn fat_image(d: &Path, fat: u32, kb: u32) -> Option<PathBuf> {
    let img = d.join("v.img");
    let ok = Command::new("mkfs.fat").args(["-C", "-F", &fat.to_string(), "-n", "CAPSAFUSE"]).arg(&img).arg(kb.to_string()).output().ok()?.status.success();
    ok.then_some(img)
}

fn bytes(seed: u64, n: usize) -> Vec<u8> {
    let mut x = seed | 1;
    (0..n).map(|_| { x ^= x << 13; x ^= x >> 7; x ^= x << 17; x as u8 }).collect()
}

#[test]
fn fat32_through_the_kernel() {
    let d = tmp("fat32");
    let Some(img) = fat_image(&d, 32, 64 * 1024) else { return eprintln!("SKIP: mkfs.fat") };
    let f = std::fs::OpenOptions::new().read(true).write(true).open(&img).unwrap();
    let v = Volume::open(f).unwrap();
    let m_dir = d.join("mnt");
    let Some(m) = Mount::new(v, &m_dir, Options { uid: 1234, gid: 5678, umask: 0o027, ..Options::default() }) else { return };
    let p = |s: &str| m_dir.join(s);
    // Files, by std::fs, through the kernel.
    std::fs::write(p("hello.txt"), b"hello, world\n").unwrap();
    let big = bytes(7, 3_000_000);
    std::fs::write(p("A long name with spaces.bin"), &big).unwrap();
    std::fs::create_dir(p("Documents")).unwrap();
    std::fs::create_dir_all(p("Documents/a/b/c")).unwrap();
    std::fs::write(p("Documents/a/b/c/deep.txt"), b"deep").unwrap();
    std::fs::write(p("Ünïcödé ✓.txt"), b"u").unwrap();
    assert_eq!(std::fs::read(p("hello.txt")).unwrap(), b"hello, world\n");
    assert_eq!(std::fs::read(p("A long name with spaces.bin")).unwrap(), big);
    // Metadata: owner from the options, sizes, kinds.
    use std::os::unix::fs::MetadataExt;
    let md = std::fs::metadata(p("hello.txt")).unwrap();
    assert_eq!((md.uid(), md.gid(), md.len(), md.mode() & 0o777), (1234, 5678, 13, 0o640));
    assert!(std::fs::metadata(p("Documents")).unwrap().is_dir());
    assert_eq!(std::fs::metadata(p("Documents")).unwrap().mode() & 0o777, 0o750, "umask 027");
    // Listing.
    let mut names: Vec<String> = std::fs::read_dir(&m_dir).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
    names.sort();
    assert_eq!(names, ["A long name with spaces.bin", "Documents", "hello.txt", "Ünïcödé ✓.txt"]);
    // Append, overwrite in place, truncate.
    use std::io::{Seek, SeekFrom, Write};
    let mut f = std::fs::OpenOptions::new().append(true).open(p("hello.txt")).unwrap();
    f.write_all(b"more\n").unwrap();
    drop(f);
    let mut f = std::fs::OpenOptions::new().write(true).open(p("hello.txt")).unwrap();
    f.seek(SeekFrom::Start(0)).unwrap();
    f.write_all(b"HELLO").unwrap();
    f.set_len(10).unwrap();
    drop(f);
    assert_eq!(std::fs::read(p("hello.txt")).unwrap(), b"HELLO, wor");
    // Rename (same dir, across dirs, replacing), remove.
    std::fs::rename(p("hello.txt"), p("Documents/greeting.txt")).unwrap();
    std::fs::write(p("old.txt"), b"old").unwrap();
    std::fs::write(p("new.txt"), b"new").unwrap();
    std::fs::rename(p("new.txt"), p("old.txt")).unwrap();
    assert_eq!(std::fs::read(p("old.txt")).unwrap(), b"new");
    assert!(std::fs::metadata(p("new.txt")).is_err());
    std::fs::rename(p("Documents/a"), p("moved a")).unwrap();
    assert_eq!(std::fs::read(p("moved a/b/c/deep.txt")).unwrap(), b"deep");
    assert!(std::fs::remove_dir(p("moved a")).is_err(), "not empty");
    std::fs::remove_file(p("moved a/b/c/deep.txt")).unwrap();
    std::fs::remove_dir(p("moved a/b/c")).unwrap();
    // Refused names come back as errors, not as damage.
    assert!(std::fs::write(p("bad:name"), b"x").is_err());
    // Case-insensitive like every FAT.
    assert_eq!(std::fs::read(p("documents/GREETING.TXT")).unwrap(), b"HELLO, wor");
    // statvfs sees the volume.
    let c = CString::new(m_dir.as_os_str().as_encoded_bytes()).unwrap();
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    assert_eq!(unsafe { libc::statvfs(c.as_ptr(), &mut s) }, 0);
    assert!(s.f_blocks > 0 && s.f_bfree < s.f_blocks);
    m.unmount().unwrap();
    // After unmount, other tools agree.
    let o = Command::new("fsck.fat").args(["-n"]).arg(&img).output().unwrap();
    assert!(o.status.success(), "fsck.fat: {}", String::from_utf8_lossy(&o.stdout));
    let o = Command::new("mtype").env("LC_ALL", "C.UTF-8").arg("-i").arg(&img).arg("::Documents/greeting.txt").output().unwrap();
    assert_eq!(o.stdout, b"HELLO, wor");
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn read_only_option() {
    let d = tmp("ro");
    let Some(img) = fat_image(&d, 16, 16 * 1024) else { return eprintln!("SKIP: mkfs.fat") };
    let o = Command::new("mcopy").arg("-i").arg(&img).arg("/etc/hostname").arg("::host.txt").output().unwrap();
    assert!(o.status.success());
    let v = Volume::open(File::open(&img).unwrap()).unwrap();
    let m_dir = d.join("mnt");
    let Some(m) = Mount::new(v, &m_dir, Options { read_only: true, ..Options::default() }) else { return };
    assert_eq!(std::fs::read(m_dir.join("host.txt")).unwrap(), std::fs::read("/etc/hostname").unwrap());
    let e = std::fs::write(m_dir.join("x"), b"x").unwrap_err();
    assert_eq!(e.raw_os_error(), Some(libc::EROFS));
    assert_eq!(std::fs::remove_file(m_dir.join("host.txt")).unwrap_err().raw_os_error(), Some(libc::EROFS));
    m.unmount().unwrap();
    std::fs::remove_dir_all(&d).unwrap();
}
