//! exFAT volumes made by exfatprogs and filled through the exfat-fuse
//! driver on a loop device, read back. Needs root, losetup and
//! mount.exfat-fuse; skipped, loudly, without them.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use capsa_fat::exfat::ExVolume;
use capsa_fat::ReadOnly;

fn sh(c: &mut Command) -> Option<String> {
    let o = c.output().ok()?;
    o.status.success().then(|| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

fn content(seed: u64, n: usize) -> Vec<u8> {
    let mut x = seed | 1;
    (0..n).map(|_| { x ^= x << 13; x ^= x >> 7; x ^= x << 17; x as u8 }).collect()
}

const FILES: &[(&str, usize)] = &[
    ("README.TXT", 100),
    ("empty", 0),
    ("A long file name with spaces, more than fifteen characters.txt", 5000),
    ("Ünïcödé — ✓ 文件.bin", 3000),
    ("docs/Notes for the trip.md", 70_000),
    ("docs/sub/deep/deeper/x.dat", 1),
    ("photos/IMG_0001.JPG", 1_500_000),
];

/// Build the image; `None` when this machine cannot.
fn build(dir: &Path) -> Option<PathBuf> {
    let img = dir.join("x.img");
    std::fs::File::create(&img).ok()?.set_len(64 << 20).ok()?;
    sh(Command::new("mkfs.exfat").args(["-L", "CAPSAX"]).arg(&img))?;
    let lo = sh(Command::new("losetup").args(["-f", "--show"]).arg(&img))?;
    let mnt = dir.join("mnt");
    std::fs::create_dir_all(&mnt).ok()?;
    let mounted = sh(Command::new("mount.exfat-fuse").arg(&lo).arg(&mnt)).is_some();
    let ok = mounted && fill(&mnt).is_some();
    if mounted {
        let _ = Command::new("sync").status();
        let _ = sh(Command::new("umount").arg(&mnt));
    }
    let _ = sh(Command::new("losetup").args(["-d", &lo]));
    ok.then_some(img)
}

fn fill(m: &Path) -> Option<()> {
    for (i, (p, n)) in FILES.iter().enumerate() {
        let f = m.join(p);
        std::fs::create_dir_all(f.parent()?).ok()?;
        std::fs::write(&f, content(i as u64, *n)).ok()?;
    }
    // Two files grown in turns: both end up fragmented, on FAT chains.
    let (a, b) = (m.join("frag-a.bin"), m.join("frag-b.bin"));
    for k in 0..6u64 {
        for (p, s) in [(&a, 100), (&b, 200)] {
            let mut f = std::fs::OpenOptions::new().create(true).append(true).open(p).ok()?;
            f.write_all(&content(s + k, 70_000)).ok()?;
        }
    }
    std::fs::create_dir_all(m.join("many")).ok()?;
    for k in 0..300 {
        std::fs::write(m.join(format!("many/many-{k:03}-with-a-long-name.txt")), format!("{k}")).ok()?;
    }
    Some(())
}

#[test]
fn exfat_reads() {
    let d = std::env::temp_dir().join(format!("capsa-exfat-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let Some(img) = build(&d) else {
        eprintln!("SKIP: cannot build an exFAT volume here (needs root, losetup, mkfs.exfat, mount.exfat-fuse)");
        return;
    };
    let bytes = std::fs::read(&img).unwrap();
    let mut v = ExVolume::open(ReadOnly(&bytes)).unwrap();
    assert_eq!(v.label.as_deref(), Some("CAPSAX"));
    for (i, (p, n)) in FILES.iter().enumerate() {
        let e = v.resolve(p).unwrap().unwrap_or_else(|| panic!("{p}"));
        assert_eq!(e.name, p.rsplit('/').next().unwrap());
        assert_eq!(e.len as usize, *n, "{p}");
        let mut b = vec![0u8; *n];
        let mut off = 0;
        while off < b.len() {
            let end = (off + 4099).min(b.len());
            off += v.read(&e, off as u64, &mut b[off..end]).unwrap();
        }
        assert_eq!(b, content(i as u64, *n), "{p}");
    }
    for (name, s) in [("frag-a.bin", 100u64), ("frag-b.bin", 200)] {
        let e = v.resolve(name).unwrap().unwrap();
        let mut want = Vec::new();
        for k in 0..6 {
            want.extend(content(s + k, 70_000));
        }
        let mut b = vec![0u8; want.len()];
        assert_eq!(v.read(&e, 0, &mut b).unwrap(), want.len());
        assert_eq!(b, want, "{name}");
    }
    assert!(!v.resolve("frag-a.bin").unwrap().unwrap().contiguous || !v.resolve("frag-b.bin").unwrap().unwrap().contiguous, "one of them is on a FAT chain");
    let many = v.resolve("many").unwrap().unwrap();
    assert_eq!(v.read_dir(many.as_dir().unwrap()).unwrap().len(), 300);
    assert!(v.resolve("readme.txt").unwrap().is_some());
    let free = v.free_clusters().unwrap();
    assert!(free > 0 && free < v.geometry().clusters);
    // Damage: never a panic or hang.
    let mut x = 0x2545F491u32;
    for round in 0..200 {
        let mut b = bytes.clone();
        for _ in 0..(1 + round % 30) {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            let span = if x % 3 == 0 { b.len() } else { 2 << 20 };
            let i = (x as usize >> 3) % span;
            b[i] = (x >> 7) as u8;
        }
        let Ok(mut v) = ExVolume::open(ReadOnly(&b)) else { continue };
        let root = v.root();
        let mut stack = vec![root];
        let mut seen = 0;
        while let Some(dd) = stack.pop() {
            seen += 1;
            if seen > 100 {
                break;
            }
            for e in v.read_dir(dd).unwrap_or_default() {
                if let Ok(sub) = e.as_dir() {
                    stack.push(sub);
                } else {
                    let _ = v.read(&e, 0, &mut [0u8; 4096]);
                }
            }
        }
        let _ = v.free_clusters();
    }
    std::fs::remove_dir_all(&d).unwrap();
}
