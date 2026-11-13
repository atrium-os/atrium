//! exFAT changes made by capsa-fat, checked by exfatprogs' fsck.exfat and
//! read back by the exfat-fuse driver (when this machine can mount it):
//! the volumes we write must be ones other systems accept.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use capsa_fat::exfat::{ExDir, ExVolume};
use capsa_fat::{Error, ReadOnly};

const NOW: i64 = 1_794_579_656;

fn sh(c: &mut Command) -> Option<String> {
    let o = c.output().ok()?;
    o.status.success().then(|| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

fn bytes(seed: u64, n: usize) -> Vec<u8> {
    let mut x = seed | 1;
    (0..n).map(|_| { x ^= x << 13; x ^= x >> 7; x ^= x << 17; x as u8 }).collect()
}

fn tools() -> bool {
    let y = Command::new("mkfs.exfat").arg("-V").output().is_ok() && Command::new("fsck.exfat").arg("-V").output().is_ok();
    if !y {
        eprintln!("SKIP: exfatprogs not installed");
    }
    y
}

fn image(name: &str, mb: u64, cluster: Option<&str>) -> PathBuf {
    let d = std::env::temp_dir().join(format!("capsa-xw-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let img = d.join("v.img");
    std::fs::File::create(&img).unwrap().set_len(mb << 20).unwrap();
    let mut c = Command::new("mkfs.exfat");
    if let Some(cs) = cluster {
        c.args(["-c", cs]);
    }
    assert!(sh(c.args(["-L", "CAPSAW"]).arg(&img)).is_some(), "mkfs.exfat");
    img
}

fn fsck(img: &Path) {
    let o = Command::new("fsck.exfat").args(["-n", "-v"]).arg(img).output().unwrap();
    let out = String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr);
    assert!(o.status.success(), "fsck.exfat: {out}");
    for bad in ["corrupt", "Corrupt", "wrong", "invalid", "Invalid", "error", "ERROR", "dirty", "mismatch", "fix"] {
        assert!(!out.contains(bad), "fsck.exfat said {bad:?}: {out}");
    }
}

/// Files and directories through exfat-fuse, as another system sees them.
/// `None` when this machine cannot mount (needs root, losetup, exfat-fuse).
fn foreign_tree(img: &Path) -> Option<BTreeMap<String, Option<Vec<u8>>>> {
    let lo = sh(Command::new("losetup").args(["-f", "--show"]).arg(img))?;
    let mnt = img.with_file_name("mnt");
    std::fs::create_dir_all(&mnt).ok()?;
    let mounted = sh(Command::new("mount.exfat-fuse").args(["-o", "ro"]).arg(&lo).arg(&mnt)).is_some();
    let mut out = BTreeMap::new();
    fn walk(base: &Path, p: &Path, out: &mut BTreeMap<String, Option<Vec<u8>>>) {
        for e in std::fs::read_dir(p).unwrap() {
            let e = e.unwrap();
            let rel = e.path().strip_prefix(base).unwrap().to_string_lossy().to_string();
            if e.file_type().unwrap().is_dir() {
                out.insert(rel, None);
                walk(base, &e.path(), out);
            } else {
                out.insert(rel, Some(std::fs::read(e.path()).unwrap()));
            }
        }
    }
    if mounted {
        walk(&mnt, &mnt, &mut out);
        let _ = sh(Command::new("umount").arg(&mnt));
    }
    let _ = sh(Command::new("losetup").args(["-d", &lo]));
    mounted.then_some(out)
}

/// The same tree as capsa-fat reads it back.
fn own_tree(img: &Path) -> BTreeMap<String, Option<Vec<u8>>> {
    let b = std::fs::read(img).unwrap();
    let mut v = ExVolume::open(ReadOnly(&b)).unwrap();
    let mut out = BTreeMap::new();
    let mut stack = vec![(v.root(), String::new())];
    while let Some((d, pre)) = stack.pop() {
        for e in v.read_dir(d).unwrap() {
            let p = format!("{pre}{}", e.name);
            if e.is_dir() {
                out.insert(p.clone(), None);
                stack.push((e.as_dir().unwrap(), format!("{p}/")));
            } else {
                let mut buf = vec![0u8; e.len as usize];
                assert_eq!(v.read(&e, 0, &mut buf).unwrap(), buf.len());
                out.insert(p, Some(buf));
            }
        }
    }
    out
}

fn dir(v: &mut ExVolume<std::fs::File>, path: &str) -> ExDir {
    match v.resolve(path).unwrap() {
        None => v.root(),
        Some(e) => e.as_dir().unwrap(),
    }
}

fn open(img: &Path) -> ExVolume<std::fs::File> {
    ExVolume::open(std::fs::OpenOptions::new().read(true).write(true).open(img).unwrap()).unwrap()
}

fn put(v: &mut ExVolume<std::fs::File>, path: &str, data: &[u8]) {
    let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
    let d = dir(v, parent);
    let mut e = match v.lookup(d, name) {
        Ok(e) => e,
        Err(Error::NotFound) => v.create(d, name, false, NOW).unwrap(),
        Err(e) => panic!("{e}"),
    };
    // In 3000-byte pieces, as a FUSE client writes.
    for (k, c) in data.chunks(3000).enumerate() {
        v.write(&mut e, (k * 3000) as u64, c, NOW).unwrap();
    }
}

#[test]
fn exfat_writes_are_clean() {
    if !tools() {
        return;
    }
    for (name, mb, cs) in [("4k", 64, None), ("512", 48, Some("512")), ("128k", 256, Some("128K"))] {
        let img = image(name, mb, cs);
        let mut want: BTreeMap<String, Option<Vec<u8>>> = BTreeMap::new();
        {
            let mut v = open(&img);
            let r = v.root();
            for (p, n) in [("hello.txt", 13usize), ("empty", 0), ("A long file name with spaces, past fifteen characters.txt", 5000), ("Ünïcödé — ✓ 文件.bin", 70_000)] {
                put(&mut v, p, &bytes(n as u64, n));
                want.insert(p.into(), Some(bytes(n as u64, n)));
            }
            v.create(r, "docs", true, NOW).unwrap();
            want.insert("docs".into(), None);
            let docs = dir(&mut v, "docs");
            v.create(docs, "deep", true, NOW).unwrap();
            want.insert("docs/deep".into(), None);
            put(&mut v, "docs/deep/x.dat", b"x");
            want.insert("docs/deep/x.dat".into(), Some(b"x".to_vec()));
            // Two files grown in turns: interleaved chains.
            let (mut a, mut b) = (Vec::new(), Vec::new());
            for k in 0..5u64 {
                a.extend(bytes(100 + k, 9000));
                b.extend(bytes(200 + k, 7000));
                put(&mut v, "frag-a.bin", &a);
                put(&mut v, "frag-b.bin", &b);
            }
            want.insert("frag-a.bin".into(), Some(a));
            want.insert("frag-b.bin".into(), Some(b));
            // A directory that must grow past its first cluster.
            v.create(r, "many", true, NOW).unwrap();
            want.insert("many".into(), None);
            for k in 0..200 {
                let p = format!("many/many-{k:03}-with-a-long-name.txt");
                put(&mut v, &p, k.to_string().as_bytes());
                want.insert(p, Some(k.to_string().into_bytes()));
            }
            // A gap: zeros between the old end and the write.
            let mut g = v.lookup(r, "hello.txt").unwrap();
            v.write(&mut g, 10_000, b"tail", NOW).unwrap();
            let mut hv = bytes(13, 13);
            hv.resize(10_000, 0);
            hv.extend_from_slice(b"tail");
            want.insert("hello.txt".into(), Some(hv));
            // Truncate down, then up (reads as zeros).
            let mut t = v.lookup(r, "Ünïcödé — ✓ 文件.bin").unwrap();
            v.truncate(&mut t, 1000, NOW).unwrap();
            v.truncate(&mut t, 50_000, NOW).unwrap();
            let mut tv = bytes(70_000, 70_000);
            tv.truncate(1000);
            tv.resize(50_000, 0);
            want.insert("Ünïcödé — ✓ 文件.bin".into(), Some(tv));
            // Renames: within a directory, case only, across, replacing.
            let e = v.lookup(r, "empty").unwrap();
            v.rename(&e, r, "Still Empty", NOW).unwrap();
            let e = v.lookup(r, "still empty").unwrap();
            v.rename(&e, r, "still empty", NOW).unwrap();
            let x = want.remove("empty").unwrap();
            want.insert("still empty".into(), x);
            let e = v.lookup(r, "frag-b.bin").unwrap();
            v.rename(&e, docs, "moved.bin", NOW).unwrap();
            let x = want.remove("frag-b.bin").unwrap();
            want.insert("docs/moved.bin".into(), x);
            let e = v.lookup(r, "frag-a.bin").unwrap();
            v.rename(&e, docs, "moved.bin", NOW).unwrap();
            let x = want.remove("frag-a.bin").unwrap();
            want.insert("docs/moved.bin".into(), x);
            // A directory moved, not below itself.
            let deep = v.lookup(docs, "deep").unwrap();
            let (de, below) = (docs_entry(&mut v), dir(&mut v, "docs/deep"));
            assert!(v.rename(&de, below, "x", NOW).is_err(), "into itself");
            v.rename(&deep, r, "deep2", NOW).unwrap();
            let x = want.remove("docs/deep").unwrap();
            want.insert("deep2".into(), x);
            let x = want.remove("docs/deep/x.dat").unwrap();
            want.insert("deep2/x.dat".into(), x);
            // Removing: a non-empty directory refuses.
            let d2 = v.lookup(r, "deep2").unwrap();
            assert_eq!(v.rmdir(&d2), Err(Error::NotEmpty));
            let dd = dir(&mut v, "deep2");
            let xd = v.lookup(dd, "x.dat").unwrap();
            v.unlink(&xd).unwrap();
            let d2 = v.lookup(r, "deep2").unwrap();
            v.rmdir(&d2).unwrap();
            want.remove("deep2/x.dat");
            want.remove("deep2");
            for k in (0..200).step_by(3) {
                let md = dir(&mut v, "many");
                let e = v.lookup(md, &format!("many-{k:03}-with-a-long-name.txt")).unwrap();
                v.unlink(&e).unwrap();
                want.remove(&format!("many/many-{k:03}-with-a-long-name.txt"));
            }
            // Names exFAT cannot hold, and duplicates (any case).
            assert_eq!(v.create(r, "a:b", false, NOW).unwrap_err(), Error::InvalidName);
            assert_eq!(v.create(r, "HELLO.TXT", false, NOW).unwrap_err(), Error::Exists);
            v.sync().unwrap();
        }
        fsck(&img);
        assert_eq!(own_tree(&img), want, "{name}: read back by capsa-fat");
        match foreign_tree(&img) {
            Some(t) => assert_eq!(t, want, "{name}: read back by exfat-fuse"),
            None => eprintln!("SKIP: exfat-fuse cross-check (needs root, losetup, mount.exfat-fuse)"),
        }
        let _ = std::fs::remove_dir_all(img.parent().unwrap());
    }
}

fn docs_entry(v: &mut ExVolume<std::fs::File>) -> capsa_fat::exfat::ExEntry {
    let r = v.root();
    v.lookup(r, "docs").unwrap()
}

/// Files another system wrote contiguously (`NoFatChain`) take appends,
/// truncation and removal; space comes back.
#[test]
fn exfat_contiguous_files_grow() {
    if !tools() {
        return;
    }
    let img = image("contig", 64, None);
    let lo = match sh(Command::new("losetup").args(["-f", "--show"]).arg(&img)) {
        Some(l) => l,
        None => return eprintln!("SKIP: needs losetup and exfat-fuse"),
    };
    let mnt = img.with_file_name("mnt");
    std::fs::create_dir_all(&mnt).unwrap();
    if sh(Command::new("mount.exfat-fuse").arg(&lo).arg(&mnt)).is_none() {
        let _ = sh(Command::new("losetup").args(["-d", &lo]));
        return eprintln!("SKIP: needs exfat-fuse");
    }
    std::fs::write(mnt.join("big.bin"), bytes(1, 300_000)).unwrap();
    std::fs::create_dir_all(mnt.join("d")).unwrap();
    std::fs::write(mnt.join("d/small.bin"), bytes(2, 9000)).unwrap();
    let _ = Command::new("sync").status();
    let _ = sh(Command::new("umount").arg(&mnt));
    let _ = sh(Command::new("losetup").args(["-d", &lo]));
    let free0;
    {
        let mut v = open(&img);
        free0 = v.free_clusters().unwrap();
        let r = v.root();
        let mut big = v.lookup(r, "big.bin").unwrap();
        assert!(big.contiguous, "exfat-fuse wrote it as one run");
        v.write(&mut big, 300_000, &bytes(3, 50_000), NOW).unwrap();
        assert!(!big.contiguous);
        let mut small = v.resolve("d/small.bin").unwrap().unwrap();
        v.truncate(&mut small, 100, NOW).unwrap();
        v.write(&mut small, 100, b"!", NOW).unwrap();
        v.sync().unwrap();
    }
    fsck(&img);
    let mut big = bytes(1, 300_000);
    big.extend(bytes(3, 50_000));
    let mut small = bytes(2, 9000);
    small.truncate(100);
    small.push(b'!');
    let t = own_tree(&img);
    assert_eq!(t["big.bin"].as_deref(), Some(&big[..]));
    assert_eq!(t["d/small.bin"].as_deref(), Some(&small[..]));
    if let Some(f) = foreign_tree(&img) {
        assert_eq!(f, t, "exfat-fuse agrees");
    }
    {
        let mut v = open(&img);
        let r = v.root();
        let b = v.lookup(r, "big.bin").unwrap();
        v.unlink(&b).unwrap();
        let s = v.resolve("d/small.bin").unwrap().unwrap();
        v.unlink(&s).unwrap();
        let d = v.lookup(r, "d").unwrap();
        v.rmdir(&d).unwrap();
        v.sync().unwrap();
        // big + small + d, all back.
        assert!(v.free_clusters().unwrap() > free0, "space comes back");
    }
    fsck(&img);
    let _ = std::fs::remove_dir_all(img.parent().unwrap());
}

/// Random operations against a model, then fsck and both readers.
#[test]
fn exfat_random_ops() {
    if !tools() {
        return;
    }
    let img = image("random", 32, Some("4K"));
    let mut want: BTreeMap<String, Option<Vec<u8>>> = BTreeMap::new();
    let mut x = 0x9E3779B97F4A7C15u64;
    let mut rnd = |n: u64| {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x % n
    };
    {
        let mut v = open(&img);
        let r = v.root();
        v.create(r, "d0", true, NOW).unwrap();
        v.create(r, "d1", true, NOW).unwrap();
        want.insert("d0".into(), None);
        want.insert("d1".into(), None);
        let dirs = ["", "d0", "d1"];
        for step in 0..600 {
            let dn = dirs[rnd(3) as usize];
            let name = format!("file-{}-with-some-length", rnd(25));
            let path = if dn.is_empty() { name.clone() } else { format!("{dn}/{name}") };
            let d = dir(&mut v, dn);
            match rnd(6) {
                0 | 1 => {
                    let off = rnd(40_000);
                    let data = bytes(step, rnd(20_000) as usize + 1);
                    let mut e = match v.lookup(d, &name) {
                        Ok(e) => e,
                        Err(_) => v.create(d, &name, false, NOW).unwrap(),
                    };
                    v.write(&mut e, off, &data, NOW).unwrap();
                    let f = want.entry(path).or_insert(Some(Vec::new())).as_mut().unwrap();
                    let end = off as usize + data.len();
                    if f.len() < end {
                        f.resize(end, 0);
                    }
                    f[off as usize..end].copy_from_slice(&data);
                }
                2 => {
                    if let Ok(mut e) = v.lookup(d, &name) {
                        let len = rnd(60_000);
                        v.truncate(&mut e, len, NOW).unwrap();
                        want.get_mut(&path).unwrap().as_mut().unwrap().resize(len as usize, 0);
                    }
                }
                3 => {
                    if let Ok(e) = v.lookup(d, &name) {
                        v.unlink(&e).unwrap();
                        want.remove(&path);
                    }
                }
                _ => {
                    if let Ok(e) = v.lookup(d, &name) {
                        let tn = dirs[rnd(3) as usize];
                        let to_name = format!("file-{}-with-some-length", rnd(25));
                        let to_path = if tn.is_empty() { to_name.clone() } else { format!("{tn}/{to_name}") };
                        let td = dir(&mut v, tn);
                        v.rename(&e, td, &to_name, NOW).unwrap();
                        let data = want.remove(&path).unwrap();
                        want.insert(to_path, data);
                    }
                }
            }
            if step % 150 == 149 {
                v.sync().unwrap();
            }
        }
        v.sync().unwrap();
    }
    fsck(&img);
    assert_eq!(own_tree(&img), want);
    if let Some(f) = foreign_tree(&img) {
        assert_eq!(f, want, "exfat-fuse agrees");
    }
    let _ = std::fs::remove_dir_all(img.parent().unwrap());
}

/// Writes to damaged volumes fail cleanly: no panic, no hang.
#[test]
fn exfat_writes_on_damage() {
    if !tools() {
        return;
    }
    let img = image("damage", 8, Some("4K"));
    {
        let mut v = open(&img);
        let r = v.root();
        v.create(r, "d", true, NOW).unwrap();
        for k in 0..40 {
            put(&mut v, &format!("d/f{k}-long-enough-name"), &bytes(k, 9000));
        }
        v.sync().unwrap();
    }
    let clean = std::fs::read(&img).unwrap();
    let mut x = 0x2545F491u32;
    for round in 0..300 {
        let mut b = clean.clone();
        for _ in 0..(1 + round % 20) {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            let i = (x as usize >> 3) % (1 << 20).min(b.len());
            b[i] = (x >> 7) as u8;
        }
        let Ok(mut v) = ExVolume::open(b) else { continue };
        let r = v.root();
        let _ = v.create(r, "new-file-here", false, NOW).and_then(|mut e| v.write(&mut e, 5000, &[7u8; 20000], NOW));
        if let Ok(Some(dd)) = v.resolve("d") {
            if let Ok(d) = dd.as_dir() {
                for e in v.read_dir(d).unwrap_or_default().into_iter().take(10) {
                    let mut e2 = e.clone();
                    let _ = v.truncate(&mut e2, 3, NOW);
                    let _ = v.rename(&e, r, "moved-out", NOW);
                    let _ = v.unlink(&e);
                }
            }
            let _ = v.rmdir(&dd);
        }
        let _ = v.sync();
    }
    let _ = std::fs::remove_dir_all(img.parent().unwrap());
}
