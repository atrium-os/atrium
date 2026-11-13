//! Grants (device-assignment.md §4.8): one server, one open volume, a
//! second FUSE mount rooted at a folder and owned by an app's uid, added
//! over the control socket. Linux, as root; skipped where mounting is not
//! possible. FreeBSD's fusefs is checked on the VM.

#![cfg(target_os = "linux")]

use std::ffi::CString;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;

use capsa_fat::Volume;
use capsa_fuse::server::serve_with;
use capsa_fuse::Options;

fn fuse_mount(dir: &Path) -> Option<File> {
    let f = std::fs::OpenOptions::new().read(true).write(true).open("/dev/fuse").ok()?;
    let src = CString::new("capsa").unwrap();
    let tgt = CString::new(dir.as_os_str().as_encoded_bytes()).unwrap();
    let ty = CString::new("fuse.capsa").unwrap();
    let data = CString::new(format!("fd={},rootmode=40000,user_id=0,group_id=0,default_permissions,allow_other", f.as_raw_fd())).unwrap();
    // SAFETY: valid C strings.
    let r = unsafe { libc::mount(src.as_ptr(), tgt.as_ptr(), ty.as_ptr(), libc::MS_NOSUID | libc::MS_NODEV, data.as_ptr().cast()) };
    if r != 0 {
        eprintln!("SKIP: mount(2) fuse: {}", std::io::Error::last_os_error());
        return None;
    }
    Some(f)
}

fn umount(dir: &Path) {
    let t = CString::new(dir.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: a valid path.
    unsafe { libc::umount2(t.as_ptr(), 0) };
}

/// Send `line` with `fd` attached, as dispensatord does.
fn send(c: &UnixStream, line: &str, fd: Option<&File>) {
    let mut iov = libc::iovec { iov_base: line.as_ptr() as *mut _, iov_len: line.len() };
    let mut cbuf = [0u64; 4];
    // SAFETY: zeroed msghdr is valid; pointers outlive the call.
    let mut m: libc::msghdr = unsafe { std::mem::zeroed() };
    m.msg_iov = &mut iov;
    m.msg_iovlen = 1;
    if let Some(f) = fd {
        m.msg_control = cbuf.as_mut_ptr().cast();
        // SAFETY: the CMSG macros on our own buffer.
        unsafe {
            m.msg_controllen = libc::CMSG_SPACE(4) as _;
            let h = libc::CMSG_FIRSTHDR(&m);
            (*h).cmsg_level = libc::SOL_SOCKET;
            (*h).cmsg_type = libc::SCM_RIGHTS;
            (*h).cmsg_len = libc::CMSG_LEN(4) as _;
            std::ptr::write_unaligned(libc::CMSG_DATA(h) as *mut i32, f.as_raw_fd());
        }
    }
    // SAFETY: m is valid.
    assert!(unsafe { libc::sendmsg(c.as_raw_fd(), &m, 0) } >= 0);
}

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("capsa-grant-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    for m in ["mnt", "app", "app2"] {
        std::fs::create_dir_all(d.join(m)).unwrap();
    }
    d
}

#[test]
fn a_folder_for_an_app() {
    let d = tmp("folder");
    let img = d.join("v.img");
    if !Command::new("mkfs.fat").args(["-C", "-F", "32", "-n", "GRANT"]).arg(&img).arg("65536").output().map(|o| o.status.success()).unwrap_or(false) {
        return eprintln!("SKIP: mkfs.fat");
    }
    let v = Volume::open(std::fs::OpenOptions::new().read(true).write(true).open(&img).unwrap()).unwrap();
    let (main, app, app2) = (d.join("mnt"), d.join("app"), d.join("app2"));
    let Some(f) = fuse_mount(&main) else { return };
    let (ours, theirs) = UnixStream::pair().unwrap();
    let th = std::thread::spawn(move || serve_with(v, f, Options { uid: 1000, gid: 1000, umask: 0o077, ..Options::default() }, Some(theirs)));
    let p = |s: &str| main.join(s);
    std::fs::create_dir_all(p("Photos/2026")).unwrap();
    std::fs::write(p("Photos/2026/a.jpg"), b"jpeg").unwrap();
    std::fs::write(p("secret.txt"), b"not for the app").unwrap();
    let mut replies = BufReader::new(ours.try_clone().unwrap());
    let mut reply = || {
        let mut l = String::new();
        replies.read_line(&mut l).unwrap();
        l
    };
    // Refusals: no fd, a path that is not there, a file, `..`.
    send(&ours, "mount 50001 50001 0 /Photos", None);
    assert!(reply().starts_with("error no /dev/fuse"));
    for bad in ["/Nope", "/secret.txt", "/Photos/../"] {
        let Some(g) = fuse_mount(&app2) else { return };
        send(&ours, &format!("mount 50001 50001 0 {bad}"), Some(&g));
        assert!(reply().starts_with("error"), "{bad}");
        drop(g);
        umount(&app2);
    }
    // The grant: Photos alone, the app's own.
    let Some(g) = fuse_mount(&app) else { return };
    send(&ours, "mount 50001 50002 0 /Photos", Some(&g));
    assert_eq!(reply(), "ok\n");
    drop(g);
    let names: Vec<String> = std::fs::read_dir(&app).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
    assert_eq!(names, ["2026"]);
    assert_eq!(std::fs::read(app.join("2026/a.jpg")).unwrap(), b"jpeg");
    let md = std::fs::metadata(app.join("2026/a.jpg")).unwrap();
    assert_eq!((md.uid(), md.gid(), md.mode() & 0o777), (50001, 50002, 0o600));
    assert_eq!(std::fs::metadata(&app).unwrap().uid(), 50001, "its root too");
    // `..` from inside leads back to its root, and from its root out of
    // the mount (the host's directory, the jail's in Atrium), never to the
    // stick's root.
    let ls = |q: &Path| -> Vec<String> { std::fs::read_dir(q).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect() };
    assert_eq!(ls(&app.join("2026/..")), ["2026"]);
    assert!(!ls(&app.join("2026/../..")).contains(&"secret.txt".to_string()));
    // Its writes land on the one volume; the session sees them at once.
    std::fs::write(app.join("2026/b.jpg"), b"new").unwrap();
    assert_eq!(std::fs::read(p("Photos/2026/b.jpg")).unwrap(), b"new");
    // The session moves a file away and removes another: the grant follows.
    std::fs::rename(p("Photos/2026/a.jpg"), p("Photos/2026/c.jpg")).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    assert_eq!(std::fs::read(app.join("2026/c.jpg")).unwrap(), b"jpeg");
    assert!(std::fs::metadata(app.join("2026/a.jpg")).is_err());
    // The grant ends alone; the session's mount goes on.
    umount(&app);
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert_eq!(std::fs::read(p("Photos/2026/b.jpg")).unwrap(), b"new");
    std::fs::write(p("after.txt"), b"still here").unwrap();
    // The whole stick, read-only, for another (the file manager's shape).
    let Some(g) = fuse_mount(&app2) else { return };
    send(&ours, "mount 50003 50003 1 /", Some(&g));
    assert_eq!(reply(), "ok\n");
    drop(g);
    assert_eq!(std::fs::read(app2.join("secret.txt")).unwrap(), b"not for the app");
    assert!(std::fs::write(app2.join("x.txt"), b"x").is_err(), "read-only grant");
    // The session's mount going ends the server, grants and all.
    umount(&main);
    th.join().unwrap().unwrap();
    umount(&app2);
    let o = Command::new("fsck.fat").arg("-n").arg(&img).output().unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stdout));
    let _ = std::fs::remove_dir_all(&d);
}
