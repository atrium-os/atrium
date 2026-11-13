//! Volumes made by dosfstools (mkfs.fat) and mtools, read back. Skipped,
//! loudly, when those tools are not installed.

use std::path::{Path, PathBuf};
use std::process::Command;

use capsa_fat::{Entry, FatType, ReadOnly, Volume};

fn have_tools() -> bool {
    let ok = |t: &str| Command::new(t).arg("--version").output().is_ok();
    let yes = ok("mkfs.fat") && ok("mcopy");
    if !yes {
        eprintln!("SKIP: mkfs.fat / mtools not installed");
    }
    yes
}

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("capsa-fat-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn run(c: &mut Command) {
    let o = c.env("LC_ALL", "C.UTF-8").output().unwrap();
    assert!(o.status.success(), "{c:?}: {}", String::from_utf8_lossy(&o.stderr));
}

/// Deterministic bytes for a file of `n` bytes.
fn content(seed: u32, n: usize) -> Vec<u8> {
    let mut x = seed.wrapping_mul(2654435761).max(1);
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect()
}

/// The tree every image gets: (path, size).
const FILES: &[(&str, usize)] = &[
    ("README.TXT", 100),
    ("empty", 0),
    ("A long file name with spaces.txt", 5000),
    ("Ünïcödé — ✓ 文件.bin", 3000),
    ("docs/Notes for the trip.md", 70_000),
    ("docs/sub/deep/deeper/x.dat", 1),
    ("photos/IMG_0001.JPG", 131_072),
    ("photos/a.b.c.d", 513),
    ("lower.txt", 10),
];

fn make(dir: &Path, fat: u32, size_kb: u32) -> PathBuf {
    let img = dir.join(format!("fat{fat}.img"));
    run(Command::new("mkfs.fat").args(["-C", "-F", &fat.to_string(), "-n", "CAPSATEST"]).arg(&img).arg(size_kb.to_string()));
    let src = dir.join("src");
    for (i, (p, n)) in FILES.iter().enumerate() {
        let f = src.join(p);
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(&f, content(i as u32, *n)).unwrap();
    }
    for d in ["docs", "docs/sub", "docs/sub/deep", "docs/sub/deep/deeper", "photos"] {
        run(Command::new("mmd").arg("-i").arg(&img).arg(format!("::{d}")));
    }
    for (p, _) in FILES {
        run(Command::new("mcopy").arg("-i").arg(&img).arg(src.join(p)).arg(format!("::{p}")));
    }
    // Many entries: a directory spanning several clusters.
    for k in 0..300 {
        let f = src.join(format!("many-{k:03}-with-a-long-name.txt"));
        std::fs::write(&f, format!("{k}")).unwrap();
    }
    run(Command::new("mmd").arg("-i").arg(&img).arg("::many"));
    let names: Vec<PathBuf> = (0..300).map(|k| src.join(format!("many-{k:03}-with-a-long-name.txt"))).collect();
    run(Command::new("mcopy").arg("-i").arg(&img).args(&names).arg("::many/"));
    img
}

fn read_all<D: capsa_fat::BlockDev>(v: &mut Volume<D>, e: &Entry) -> Vec<u8> {
    let mut out = vec![0u8; e.size as usize];
    // Odd-sized reads at odd offsets, to cross cluster edges.
    let mut off = 0usize;
    let mut step = 777usize;
    while off < out.len() {
        let end = (off + step).min(out.len());
        let n = v.read(e, off as u64, &mut out[off..end]).unwrap();
        assert!(n > 0);
        off += n;
        step = step * 3 % 4099 + 1;
    }
    out
}

fn check(fat: u32, size_kb: u32, want: FatType) {
    if !have_tools() {
        return;
    }
    let d = tmp(&format!("fat{fat}"));
    let img = make(&d, fat, size_kb);
    let bytes = std::fs::read(&img).unwrap();
    let mut v = Volume::open(ReadOnly(&bytes)).unwrap();
    assert_eq!(v.geometry().fat_type, want);
    assert_eq!(v.stat().unwrap().label.as_deref(), Some("CAPSATEST"));
    for (i, (p, n)) in FILES.iter().enumerate() {
        let e = v.resolve(p).unwrap().unwrap_or_else(|| panic!("{p}"));
        assert_eq!(e.name, p.rsplit('/').next().unwrap(), "long name kept");
        assert_eq!(e.size as usize, *n, "{p}");
        assert_eq!(read_all(&mut v, &e), content(i as u32, *n), "{p}");
        assert!(e.mtime > 1_600_000_000, "{p}: mtime {}", e.mtime);
    }
    // Case-insensitive, by long and by 8.3 name.
    assert!(v.resolve("readme.txt").unwrap().is_some());
    assert!(v.resolve("DOCS/NOTES FOR THE TRIP.MD").unwrap().is_some());
    assert!(v.resolve("docs/NOTESF~1.MD").unwrap().is_some());
    assert_eq!(v.resolve("nope").unwrap_err(), capsa_fat::Error::NotFound);
    let root = v.root();
    let names: Vec<String> = v.read_dir(root).unwrap().into_iter().map(|e| e.name).collect();
    for want in ["README.TXT", "docs", "photos", "many", "empty"] {
        assert!(names.iter().any(|n| n == want), "{want} in {names:?}");
    }
    let many = v.resolve("many").unwrap().unwrap();
    let md = v.dir_of(&many).unwrap();
    let list = v.read_dir(md).unwrap();
    assert_eq!(list.len(), 300);
    assert!(list.iter().all(|e| e.name.starts_with("many-") && e.name.ends_with("-with-a-long-name.txt")));
    // Reads past the end, and of directories.
    let e = v.resolve("README.TXT").unwrap().unwrap();
    assert_eq!(v.read(&e, 100, &mut [0u8; 8]).unwrap(), 0);
    assert_eq!(v.read(&many, 0, &mut [0u8; 8]).unwrap_err(), capsa_fat::Error::IsDir);
    // Free space agrees with fsck's idea of it roughly: something is used.
    let st = v.stat().unwrap();
    assert!(st.free < st.clusters && st.free > 0);
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn fat12() {
    check(12, 4096, FatType::Fat12);
}

#[test]
fn fat16() {
    check(16, 32 * 1024, FatType::Fat16);
}

#[test]
fn fat32() {
    check(32, 64 * 1024, FatType::Fat32);
}

/// Random damage to a real volume: every operation may fail, none may
/// panic or hang.
#[test]
fn damaged_volumes_never_panic() {
    if !have_tools() {
        return;
    }
    let d = tmp("damage");
    let img = make(&d, 12, 4096);
    let good = std::fs::read(&img).unwrap();
    let mut x = 0x9E3779B9u32;
    let mut rnd = || {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        x
    };
    for round in 0..300 {
        let mut b = good.clone();
        // Damage the metadata-heavy first 64 KiB mostly.
        for _ in 0..(1 + round % 40) {
            let span = if rnd() % 4 == 0 { b.len() } else { 65536 };
            let i = rnd() as usize % span;
            b[i] = rnd() as u8;
        }
        let Ok(mut v) = Volume::open(ReadOnly(&b)) else { continue };
        walk(&mut v, 0);
    }
    std::fs::remove_dir_all(&d).unwrap();
}

fn walk<D: capsa_fat::BlockDev>(v: &mut Volume<D>, depth: u32) {
    let root = v.root();
    let mut stack = vec![(root, 0u32)];
    let mut seen = 0;
    while let Some((dir, depth)) = stack.pop() {
        seen += 1;
        if seen > 200 || depth > 8 {
            break;
        }
        let Ok(list) = v.read_dir(dir) else { continue };
        for e in list {
            if e.is_dir() {
                if let Ok(d) = v.dir_of(&e) {
                    stack.push((d, depth + 1));
                }
            } else {
                let mut buf = vec![0u8; 4096];
                let _ = v.read(&e, 0, &mut buf);
                let _ = v.read(&e, u64::from(e.size).saturating_sub(10), &mut buf);
            }
        }
    }
    let _ = v.stat();
    let _ = depth;
}
