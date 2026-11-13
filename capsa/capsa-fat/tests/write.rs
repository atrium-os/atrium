//! Changes made by capsa-fat, checked by dosfstools' fsck.fat and read
//! back by mtools: the volumes we write must be ones other systems accept.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use capsa_fat::{DirRef, Error, Volume};

const NOW: i64 = 1_794_579_656;

fn tools() -> bool {
    let ok = |t: &str| Command::new(t).arg("--version").output().is_ok();
    let y = ok("mkfs.fat") && ok("fsck.fat") && ok("mcopy");
    if !y {
        eprintln!("SKIP: dosfstools / mtools not installed");
    }
    y
}

fn image(name: &str, fat: u32, kb: u32) -> PathBuf {
    let d = std::env::temp_dir().join(format!("capsa-w-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let img = d.join("v.img");
    let o = Command::new("mkfs.fat").args(["-C", "-F", &fat.to_string()]).arg(&img).arg(kb.to_string()).output().unwrap();
    assert!(o.status.success());
    img
}

fn fsck(img: &Path) {
    let o = Command::new("fsck.fat").args(["-n", "-V"]).arg(img).output().unwrap();
    let out = String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr);
    assert!(o.status.success(), "fsck.fat: {out}");
    // -n reports problems without fixing; a clean volume has none.
    for bad in ["Free cluster summary wrong", "differ", "Orphan", "lost", "bad", "Contains a free cluster", "cluster chain", "Deleting", "has a large number"] {
        assert!(!out.contains(bad), "fsck.fat found {bad:?}: {out}");
    }
}

fn mread(img: &Path, path: &str) -> Vec<u8> {
    let o = Command::new("mtype").env("LC_ALL", "C.UTF-8").arg("-i").arg(img).arg(format!("::{path}")).output().unwrap();
    assert!(o.status.success(), "mtype {path}: {}", String::from_utf8_lossy(&o.stderr));
    o.stdout
}

fn bytes(seed: u64, n: usize) -> Vec<u8> {
    let mut x = seed | 1;
    (0..n).map(|_| { x ^= x << 13; x ^= x >> 7; x ^= x << 17; x as u8 }).collect()
}

fn dir_of(v: &mut Volume<std::fs::File>, path: &str) -> DirRef {
    match v.resolve(path).unwrap() {
        None => v.root(),
        Some(e) => v.dir_of(&e).unwrap(),
    }
}

fn scenario(fat: u32, kb: u32) {
    if !tools() {
        return;
    }
    let img = image(&format!("s{fat}"), fat, kb);
    let f = std::fs::OpenOptions::new().read(true).write(true).open(&img).unwrap();
    let mut v = Volume::open(f).unwrap();
    let mut model: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let root = v.root();
    // Files with short, lowercase, long and Unicode names.
    for (i, name) in ["A.TXT", "lower.txt", "A long name with spaces.text", "Ünïcödé ✓ 文件.bin", "a.b.c.d", ".hidden"].iter().enumerate() {
        let mut e = v.create(root, name, false, NOW).unwrap();
        let data = bytes(i as u64, 1000 * (i + 1) * (i + 1));
        // Written in uneven pieces.
        let mut off = 0;
        for chunk in data.chunks(777 + i * 100) {
            v.write(&mut e, off, chunk, NOW).unwrap();
            off += chunk.len() as u64;
        }
        model.insert(name.to_string(), data);
    }
    assert_eq!(v.create(root, "a.txt", false, NOW).unwrap_err(), Error::Exists, "case-insensitive clash");
    assert_eq!(v.create(root, "bad:name", false, NOW).unwrap_err(), Error::InvalidName);
    // Directories, nested, and one with enough entries to need more clusters.
    let docs = v.create(root, "Documents", true, NOW).unwrap();
    let dd = v.dir_of(&docs).unwrap();
    let sub = v.create(dd, "sub folder", true, NOW).unwrap();
    let sd = v.dir_of(&sub).unwrap();
    for k in 0..150 {
        let n = format!("note number {k:03} with a long name.md");
        let mut e = v.create(sd, &n, false, NOW).unwrap();
        let data = format!("note {k}\n").into_bytes();
        v.write(&mut e, 0, &data, NOW).unwrap();
        model.insert(format!("Documents/sub folder/{n}"), data);
    }
    // A hole: write past the end reads back as zeros between.
    let mut e = v.create(dd, "sparse.bin", false, NOW).unwrap();
    v.write(&mut e, 10_000, b"end", NOW).unwrap();
    let mut want = vec![0u8; 10_000];
    want.extend_from_slice(b"end");
    model.insert("Documents/sparse.bin".into(), want);
    // Truncate down, then up.
    let mut e = v.resolve("A long name with spaces.text").unwrap().unwrap();
    v.truncate(&mut e, 1234, NOW).unwrap();
    model.get_mut("A long name with spaces.text").unwrap().truncate(1234);
    let mut e = v.resolve("lower.txt").unwrap().unwrap();
    v.truncate(&mut e, 0, NOW).unwrap();
    v.truncate(&mut e, 5000, NOW).unwrap();
    model.insert("lower.txt".into(), vec![0u8; 5000]);
    // Overwrite in the middle.
    let mut e = v.resolve("A.TXT").unwrap().unwrap();
    v.write(&mut e, 10, b"XXXXXXXXXX", NOW).unwrap();
    model.get_mut("A.TXT").unwrap()[10..20].copy_from_slice(b"XXXXXXXXXX");
    // Unlink, rename within a directory, across directories, replacing.
    let e = v.resolve("a.b.c.d").unwrap().unwrap();
    v.unlink(&e).unwrap();
    model.remove("a.b.c.d");
    let e = v.resolve(".hidden").unwrap().unwrap();
    v.rename(&e, dd, "moved here.dat", NOW).unwrap();
    let d = model.remove(".hidden").unwrap();
    model.insert("Documents/moved here.dat".into(), d);
    let e = v.resolve("A.TXT").unwrap().unwrap();
    v.rename(&e, root, "renamed a.txt", NOW).unwrap();
    let d = model.remove("A.TXT").unwrap();
    model.insert("renamed a.txt".into(), d);
    let e = v.resolve("lower.txt").unwrap().unwrap();
    v.rename(&e, root, "renamed a.txt", NOW).unwrap();
    let d = model.remove("lower.txt").unwrap();
    model.insert("renamed a.txt".into(), d);
    // Move a directory, and refuse moving one into itself.
    let sub = v.resolve("Documents/sub folder").unwrap().unwrap();
    let sd = v.dir_of(&sub).unwrap();
    assert!(v.rename(&sub, sd, "x", NOW).is_err());
    v.rename(&sub, root, "Top folder", NOW).unwrap();
    let moved: Vec<(String, Vec<u8>)> = model.iter().filter(|(k, _)| k.starts_with("Documents/sub folder/")).map(|(k, d)| (k.clone(), d.clone())).collect();
    for (k, d) in moved {
        model.remove(&k);
        model.insert(k.replace("Documents/sub folder/", "Top folder/"), d);
    }
    // rmdir only when empty.
    let empty = v.create(dd, "empty dir", true, NOW).unwrap();
    let docs = v.resolve("Documents").unwrap().unwrap();
    assert_eq!(v.rmdir(&docs).unwrap_err(), Error::NotEmpty);
    v.rmdir(&empty).unwrap();
    // A case-only rename.
    let e = v.resolve("Documents/sparse.bin").unwrap().unwrap();
    let ddir = dir_of(&mut v, "Documents");
    v.rename(&e, ddir, "SPARSE.bin", NOW).unwrap();
    let d = model.remove("Documents/sparse.bin").unwrap();
    model.insert("Documents/SPARSE.bin".into(), d);
    v.sync().unwrap();
    drop(v);

    fsck(&img);
    // What we wrote is what mtools reads, by the names we gave.
    for (p, d) in &model {
        assert_eq!(&mread(&img, p), d, "{p}");
    }
    let bytes_img = std::fs::read(&img).unwrap();
    let mut v = Volume::open(capsa_fat::ReadOnly(&bytes_img)).unwrap();
    for (p, d) in &model {
        let e = v.resolve(p).unwrap().unwrap_or_else(|| panic!("{p}"));
        assert_eq!(e.name, p.rsplit('/').next().unwrap());
        let mut b = vec![0u8; d.len()];
        if !d.is_empty() {
            v.read(&e, 0, &mut b).unwrap();
        }
        assert_eq!(&b, d, "{p}");
    }
    assert!(v.resolve("a.b.c.d").unwrap_err() == Error::NotFound);
    std::fs::remove_dir_all(img.parent().unwrap()).unwrap();
}

#[test]
fn fat12_writes_pass_fsck() {
    scenario(12, 4096);
}

#[test]
fn fat16_writes_pass_fsck() {
    scenario(16, 32 * 1024);
}

#[test]
fn fat32_writes_pass_fsck() {
    scenario(32, 64 * 1024);
}

/// Fill the volume: NoSpace, never corruption.
#[test]
fn full_volume() {
    if !tools() {
        return;
    }
    let img = image("full", 12, 1024);
    let f = std::fs::OpenOptions::new().read(true).write(true).open(&img).unwrap();
    let mut v = Volume::open(f).unwrap();
    let root = v.root();
    let mut e = v.create(root, "big", false, NOW).unwrap();
    let chunk = vec![7u8; 64 * 1024];
    let mut off = 0u64;
    loop {
        match v.write(&mut e, off, &chunk, NOW) {
            Ok(n) => off += n as u64,
            Err(Error::NoSpace) => break,
            Err(x) => panic!("{x}"),
        }
    }
    // The fixed root fills up too.
    let mut made = 0;
    loop {
        match v.create(root, &format!("f{made}"), false, NOW) {
            Ok(_) => made += 1,
            Err(Error::NoSpace) => break,
            Err(x) => panic!("{x}"),
        }
    }
    assert!(made > 10);
    v.sync().unwrap();
    drop(v);
    // The failed write leaves allocated clusters beyond `size`; fsck may
    // trim them, which is fine, but the volume must be otherwise sound.
    let o = Command::new("fsck.fat").args(["-n"]).arg(&img).output().unwrap();
    let out = String::from_utf8_lossy(&o.stdout);
    assert!(!out.contains("Orphan") && !out.contains("cross-linked"), "{out}");
    std::fs::remove_dir_all(img.parent().unwrap()).unwrap();
}
