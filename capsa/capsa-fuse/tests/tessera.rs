//! Tessera sticks, read-only: volumes made by mkfs-tessera (inline and
//! chunked seed files), read through the backend, through a real FUSE
//! mount, and after damage. mkfs-tessera comes from atrium-tessera/rs
//! (`cargo build -p tessera-tools` there); skipped, loudly, without it.

#![cfg(feature = "tessera")]

use std::path::PathBuf;
use std::process::Command;

use capsa_fuse::tessera::{is_tessera, TesseraVolume};
use capsa_fuse::Backend;

fn mkfs() -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../atrium-tessera/rs/target/debug/mkfs-tessera");
    p.exists().then_some(p)
}

fn content(n: usize) -> String {
    // Printable, so it fits mkfs-tessera's --seed-content.
    (0..n).map(|i| (b'a' + ((i * 7 + i / 13) % 26) as u8) as char).collect()
}

fn image(name: &str, seed: &str, body: &str, chunk: u32) -> Option<PathBuf> {
    let d = std::env::temp_dir().join(format!("capsa-t-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).ok()?;
    let img = d.join("t.img");
    let o = Command::new(mkfs()?)
        .args(["--create", "-s", "64", "--seed-file", seed, "--seed-content", body, "--seed-chunk-size", &chunk.to_string()])
        .arg(&img)
        .output()
        .ok()?;
    assert!(o.status.success(), "mkfs-tessera: {}", String::from_utf8_lossy(&o.stderr));
    Some(img)
}

fn read_all(v: &mut TesseraVolume<std::fs::File>, n: &capsa_fuse::tessera::Node, step: usize) -> Vec<u8> {
    let mut out = vec![0u8; n.attr.size as usize];
    let mut off = 0;
    while off < out.len() {
        let end = (off + step).min(out.len());
        let got = v.read(n, off as u64, &mut out[off..end]).unwrap();
        assert!(got > 0);
        off += got;
    }
    out
}

#[test]
fn tessera_files_read_back() {
    let Some(_) = mkfs() else { return eprintln!("SKIP: mkfs-tessera not built (atrium-tessera/rs: cargo build -p tessera-tools)") };
    for (name, body, chunk) in [("inline", content(18), 0u32), ("chunked", content(100_000), 16384)] {
        let img = image(name, "Hello Tessera.txt", &body, chunk).unwrap();
        let mut f = std::fs::File::open(&img).unwrap();
        assert!(is_tessera(&mut f));
        let mut v = TesseraVolume::open(f).unwrap();
        let root = v.root();
        let list = v.list(root).unwrap();
        assert_eq!(list.iter().map(|n| n.name.as_str()).collect::<Vec<_>>(), ["Hello Tessera.txt"], "{name}");
        let n = v.lookup(root, "Hello Tessera.txt").unwrap();
        assert_eq!(n.attr.size as usize, body.len());
        assert!(!n.attr.dir && n.attr.read_only);
        assert_eq!(read_all(&mut v, &n, 4099), body.as_bytes(), "{name}");
        assert_eq!(read_all(&mut v, &n, 1 << 20), body.as_bytes(), "{name}, one read");
        assert!(v.lookup(root, "hello tessera.txt").is_err(), "case-sensitive");
        assert!(!v.writable());
        let k = <TesseraVolume<std::fs::File> as Backend>::key(&n);
        assert_eq!(v.refresh(k).unwrap().attr.size, n.attr.size);
        let _ = std::fs::remove_dir_all(img.parent().unwrap());
    }
    let mut fat = vec![0u8; 1 << 20];
    assert!(!is_tessera(&mut fat));
}

/// Damaged volumes: an error or a short listing, never a crash or hang.
#[test]
fn tessera_damage() {
    let Some(_) = mkfs() else { return eprintln!("SKIP: mkfs-tessera not built") };
    let img = image("damage", "f.bin", &content(60_000), 8192).unwrap();
    let clean = std::fs::read(&img).unwrap();
    // Where the volume's bytes are: corrupt only sectors that hold some.
    let live: Vec<usize> = (0..clean.len() / 4096).filter(|s| clean[s * 4096..(s + 1) * 4096].iter().any(|&b| b != 0)).collect();
    assert!(!live.is_empty());
    let mut x = 0x2545F491u32;
    let tmp = img.with_file_name("d.img");
    let mut opened = 0;
    for round in 0..300 {
        let mut b = clean.clone();
        for _ in 0..(1 + round % 12) {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            let s = live[(x as usize >> 4) % live.len()];
            let i = s * 4096 + (x as usize >> 9) % 4096;
            b[i] = (x >> 3) as u8;
        }
        std::fs::write(&tmp, &b).unwrap();
        let Ok(mut v) = TesseraVolume::open(std::fs::File::open(&tmp).unwrap()) else { continue };
        opened += 1;
        let root = v.root();
        for n in v.list(root).unwrap_or_default() {
            let mut buf = vec![0u8; 70_000];
            let _ = v.read(&n, 0, &mut buf);
        }
        let _ = v.lookup(root, "f.bin");
    }
    assert!(opened > 0, "some damage leaves it openable");
    let _ = std::fs::remove_dir_all(img.parent().unwrap());
}

/// Through the kernel: readable, and every write refused.
#[cfg(target_os = "linux")]
#[test]
fn tessera_through_the_kernel() {
    use std::os::fd::AsRawFd;
    let Some(_) = mkfs() else { return eprintln!("SKIP: mkfs-tessera not built") };
    let body = content(70_000);
    let img = image("kernel", "notes.txt", &body, 16384).unwrap();
    let mnt = img.with_file_name("mnt");
    std::fs::create_dir_all(&mnt).unwrap();
    let Ok(fuse) = std::fs::OpenOptions::new().read(true).write(true).open("/dev/fuse") else { return eprintln!("SKIP: no /dev/fuse") };
    let data = format!("fd={},rootmode=40000,user_id=0,group_id=0,allow_other,default_permissions", fuse.as_raw_fd());
    let src = std::ffi::CString::new("capsa").unwrap();
    let tgt = std::ffi::CString::new(mnt.as_os_str().as_encoded_bytes()).unwrap();
    let ty = std::ffi::CString::new("fuse.capsa").unwrap();
    let dat = std::ffi::CString::new(data).unwrap();
    // SAFETY: valid C strings.
    if unsafe { libc::mount(src.as_ptr(), tgt.as_ptr(), ty.as_ptr(), libc::MS_NOSUID | libc::MS_NODEV, dat.as_ptr() as *const _) } != 0 {
        return eprintln!("SKIP: mount: {}", std::io::Error::last_os_error());
    }
    let v = TesseraVolume::open(std::fs::File::open(&img).unwrap()).unwrap();
    let opts = capsa_fuse::Options { uid: 1234, gid: 5678, umask: 0o077, read_only: true, ..capsa_fuse::Options::default() };
    let th = std::thread::spawn(move || capsa_fuse::serve(v, fuse, opts));
    let p = |s: &str| mnt.join(s);
    let ok = std::fs::read(p("notes.txt")).map(|b| b == body.as_bytes());
    let wr = std::fs::write(p("new.txt"), b"x");
    let un = std::fs::remove_file(p("notes.txt"));
    let md = std::fs::metadata(p("notes.txt")).map(|m| { use std::os::unix::fs::MetadataExt; (m.uid(), m.mode() & 0o777) });
    let _ = Command::new("umount").arg(&mnt).status();
    let _ = th.join();
    assert!(matches!(ok, Ok(true)), "read: {ok:?}");
    assert!(wr.is_err() && un.is_err(), "writes refused: {wr:?} {un:?}");
    assert_eq!(md.unwrap(), (1234, 0o400));
    let _ = std::fs::remove_dir_all(img.parent().unwrap());
}
